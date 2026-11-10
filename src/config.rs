//! The `--config` TOML's typed schema: one Pinboard destination, the hook, and per-source
//! account arrays, whose secrets sit inline or behind a `*_file` path and resolve in
//! `secrets`. Every field is optional, so an empty config means the defaults.

use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroUsize;
use std::str::FromStr;

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;

use crate::github::gists::{GistConfig, GistPostDate};
use crate::github::GitHubConfig;
use crate::hackernews::HackerNewsConfig;
use crate::reddit::model::RedditConfig;

/// The parsed config file.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub hooks: Hooks,
    #[serde(default)]
    pub pinboard: PinboardConfig,
    #[serde(default)]
    pub backup: BackupConfig,
    /// The tier between the globals and each account.
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub reddit: Vec<RedditAccount>,
    #[serde(default)]
    pub github: Vec<GitHubAccount>,
    #[serde(default)]
    pub hackernews: Vec<HackerNewsAccount>,
}

/// `[defaults.<source>]`: each field over the global and under the account's.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    #[serde(default)]
    pub reddit: SourceDefaults,
    #[serde(default)]
    pub github: GitHubDefaults,
    #[serde(default)]
    pub hackernews: SourceDefaults,
}

/// The overridable settings of one source; `None` falls through to the global.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceDefaults {
    pub toread: Option<bool>,
    pub public: Option<bool>,
    pub limit: Option<SyncLimit>,
    pub on_auth_failure: Option<String>,
    pub use_post_date: Option<bool>,
    pub post_date_max_age_days: Option<u64>,
    pub cleanup_stale_to_now: Option<bool>,
}

/// `[defaults.github]`: the [`SourceDefaults`] every source has, plus GitHub's own.
#[derive(Debug, Default, Deserialize)]
#[serde(from = "GitHubDefaultsTable")]
pub struct GitHubDefaults {
    pub common: SourceDefaults,
    pub gist_post_date: Option<GistPostDate>,
}

/// `[defaults.github]` as written: [`SourceDefaults`]' keys spelled out beside GitHub's
/// own, since `#[serde(flatten)]` can't be combined with `deny_unknown_fields`.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct GitHubDefaultsTable {
    toread: Option<bool>,
    public: Option<bool>,
    limit: Option<SyncLimit>,
    on_auth_failure: Option<String>,
    use_post_date: Option<bool>,
    post_date_max_age_days: Option<u64>,
    cleanup_stale_to_now: Option<bool>,
    gist_post_date: Option<GistPostDate>,
}

impl From<GitHubDefaultsTable> for GitHubDefaults {
    fn from(table: GitHubDefaultsTable) -> Self {
        let GitHubDefaultsTable {
            toread,
            public,
            limit,
            on_auth_failure,
            use_post_date,
            post_date_max_age_days,
            cleanup_stale_to_now,
            gist_post_date,
        } = table;
        Self {
            common: SourceDefaults {
                toread,
                public,
                limit,
                on_auth_failure,
                use_post_date,
                post_date_max_age_days,
                cleanup_stale_to_now,
            },
            gist_post_date,
        }
    }
}

/// `backup` output settings. The directory lives in the config because the NixOS timer
/// runs a bare `backup --all` with nowhere to put a flag.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupConfig {
    /// The snapshot directory, overwritten in place; unset, `--out` is required.
    pub directory: Option<String>,
}

/// Cross-cutting hooks.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    /// Shell command run on a re-auth failure.
    pub on_auth_failure: Option<String>,
}

/// The shared Pinboard destination.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
#[expect(
    clippy::struct_excessive_bools,
    reason = "each bool is a key of the [pinboard] table"
)]
pub struct PinboardConfig {
    pub token: Option<String>,
    pub token_file: Option<String>,
    /// Write bookmarks public (default private).
    #[serde(default)]
    pub public: bool,
    /// Mark new bookmarks to-read (default false).
    #[serde(default)]
    pub toread: bool,
    /// Seconds to pause between `posts/add` writes (default 3, what Pinboard asks for).
    pub rate_limit_secs: Option<u64>,
    /// The global [`SyncLimit`]; unset is `auto`.
    pub limit: Option<SyncLimit>,
    /// Date a bookmark by the source post date (default false).
    #[serde(default)]
    pub use_post_date: bool,
    /// With `use_post_date`, backdate only posts at most this many days old (unset,
    /// [`DEFAULT_MAX_AGE_DAYS`]); older ones are dated now (sync) or kept (cleanup).
    pub post_date_max_age_days: Option<u64>,
    /// With `use_post_date`, whether `cleanup` re-dates posts older than the cap to now
    /// (default false, which keeps cleanup idempotent).
    #[serde(default)]
    pub cleanup_stale_to_now: bool,
}

/// The backdate window in days when no tier sets `post_date_max_age_days`.
pub const DEFAULT_MAX_AGE_DAYS: u64 = 30;

/// How much one `sync` run writes and reads (`limit`, `--limit`): `auto`, `all` or a
/// positive number; `0` is refused rather than mapped to either word.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SyncLimit {
    /// No write cap; a source may stop paging once it reaches items already synced.
    #[default]
    Auto,
    /// No write cap, and every page of the source is read.
    All,
    /// Write at most this many new bookmarks, reading every page: the ones a capped run
    /// leaves unwritten sit behind pages that already look synced.
    Max(NonZeroUsize),
}

/// The error for a malformed `limit`, naming the field, never the value: it could be a
/// credential pasted into the wrong key.
const SYNC_LIMIT_EXPECTED: &str = "`limit` must be `auto` (no cap; stop paging once \
     caught up), `all` (no cap; read every page) or a positive number of bookmarks";

impl SyncLimit {
    fn from_count<E: serde::de::Error>(count: impl TryInto<usize>) -> Result<Self, E> {
        count
            .try_into()
            .ok()
            .and_then(NonZeroUsize::new)
            .map(Self::Max)
            .ok_or_else(|| E::custom(SYNC_LIMIT_EXPECTED))
    }

    /// The cap on new bookmarks written, if any.
    pub fn write_cap(self) -> Option<NonZeroUsize> {
        match self {
            Self::Max(cap) => Some(cap),
            Self::Auto | Self::All => None,
        }
    }
}

impl FromStr for SyncLimit {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.trim() {
            "auto" => Ok(Self::Auto),
            "all" => Ok(Self::All),
            #[expect(
                clippy::map_err_ignore,
                reason = "the accepted values say more than why a number didn't parse"
            )]
            count => count
                .parse::<NonZeroUsize>()
                .map(Self::Max)
                .map_err(|_| SYNC_LIMIT_EXPECTED.to_string()),
        }
    }
}

impl<'de> Deserialize<'de> for SyncLimit {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct SyncLimitVisitor;

        impl serde::de::Visitor<'_> for SyncLimitVisitor {
            type Value = SyncLimit;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("`limit` as `auto`, `all` or a positive number of bookmarks")
            }

            fn visit_u64<E: serde::de::Error>(self, count: u64) -> Result<SyncLimit, E> {
                SyncLimit::from_count(count)
            }

            fn visit_i64<E: serde::de::Error>(self, count: i64) -> Result<SyncLimit, E> {
                SyncLimit::from_count(count)
            }

            fn visit_f64<E: serde::de::Error>(self, _: f64) -> Result<SyncLimit, E> {
                Err(E::custom(SYNC_LIMIT_EXPECTED))
            }

            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<SyncLimit, E> {
                text.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_any(SyncLimitVisitor)
    }
}

/// One Reddit account: whose saves to read, the cookie, and the domain and tags.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RedditAccount {
    pub name: Option<String>,
    pub username: Option<String>,
    pub cookie: Option<String>,
    pub cookie_file: Option<String>,
    pub on_auth_failure: Option<String>,
    pub limit: Option<SyncLimit>,
    /// Override `[pinboard].toread` for this account's new bookmarks.
    pub toread: Option<bool>,
    /// Override `[pinboard].public` for this account's new bookmarks.
    pub public: Option<bool>,
    /// Override `use_post_date` for this account.
    pub use_post_date: Option<bool>,
    /// Override `post_date_max_age_days` for this account.
    pub post_date_max_age_days: Option<u64>,
    /// Override `cleanup_stale_to_now` for this account.
    pub cleanup_stale_to_now: Option<bool>,
    // Non-secret tag/domain config (`tag_*`), each defaulting in
    // `From<&RedditAccount> for RedditConfig`.
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

    /// Every configured Reddit account's `reddit_domain`, defaults included.
    pub fn reddit_domains(&self) -> Vec<String> {
        self.reddit
            .iter()
            .map(|account| RedditConfig::from(account).domain)
            .collect()
    }

    /// Parse and [validate](Self::validate) a config.
    pub fn parse(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text)
            .map_err(|e| anyhow!("parsing config: {}", describe_toml_error(&e, text)))?;
        config.validate()?;
        Ok(config)
    }

    /// Reject what parses but can't be used: a blank `backup.directory`, two accounts of a
    /// source sharing a name or backup slug, dating settings that re-date on every cleanup
    /// (for the implicit account too), a malformed `reddit_domain`, and a tag with
    /// whitespace or a comma, which would silently corrupt every bookmark it tags.
    fn validate(&self) -> Result<()> {
        const NAME_OR_USERNAME: &str = "`name` (or, with no `name`, `username`)";
        if self
            .backup
            .directory
            .as_deref()
            .is_some_and(|dir| dir.trim().is_empty())
        {
            bail!("backup.directory must not be empty");
        }
        check_unique_names(&self.reddit)?;
        check_distinct_slugs(&self.reddit, NAME_OR_USERNAME)?;
        check_unique_names(&self.github)?;
        check_distinct_slugs(&self.github, "`name`")?;
        check_unique_names(&self.hackernews)?;
        check_distinct_slugs(&self.hackernews, NAME_OR_USERNAME)?;
        self.check_stale_to_now_cap(&self.reddit)?;
        self.check_stale_to_now_cap(&self.github)?;
        self.check_stale_to_now_cap(&self.hackernews)?;

        for (index, account) in self.reddit.iter().enumerate() {
            let at = |field: &str| format!("{}[{index}].{field}", RedditAccount::SOURCE);
            check_domain(&at("reddit_domain"), account.reddit_domain.as_deref())?;
            check_tags(&at("tags"), account.tags.as_deref())?;
            check_tag(
                &at("tag_subreddit_prefix"),
                account.tag_subreddit_prefix.as_deref(),
            )?;
            check_tag(&at("tag_comment"), account.tag_comment.as_deref())?;
            check_tag(&at("tag_nsfw"), account.tag_nsfw.as_deref())?;
            check_tag(
                &at("tag_author_prefix"),
                account.tag_author_prefix.as_deref(),
            )?;
            check_tag(&at("tag_flair_prefix"), account.tag_flair_prefix.as_deref())?;
            check_tag(&at("tag_media_prefix"), account.tag_media_prefix.as_deref())?;
        }
        for (index, account) in self.github.iter().enumerate() {
            let at = |field: &str| format!("{}[{index}].{field}", GitHubAccount::SOURCE);
            check_tags(&at("tags"), account.tags.as_deref())?;
            check_tag(&at("tag_lang_prefix"), account.tag_lang_prefix.as_deref())?;
            check_tag(
                &at("tag_author_prefix"),
                account.tag_author_prefix.as_deref(),
            )?;
            check_tags(&at("gist_tags"), account.gist_tags.as_deref())?;
            check_tag(
                &at("gist_tag_lang_prefix"),
                account.gist_tag_lang_prefix.as_deref(),
            )?;
            check_tag(
                &at("gist_tag_author_prefix"),
                account.gist_tag_author_prefix.as_deref(),
            )?;
        }
        for (index, account) in self.hackernews.iter().enumerate() {
            let at = |field: &str| format!("{}[{index}].{field}", HackerNewsAccount::SOURCE);
            check_tags(&at("tags"), account.tags.as_deref())?;
            check_tag(&at("tag_comment"), account.tag_comment.as_deref())?;
            check_tag(
                &at("tag_author_prefix"),
                account.tag_author_prefix.as_deref(),
            )?;
            check_tag(
                &at("tag_special_prefix"),
                account.tag_special_prefix.as_deref(),
            )?;
            check_tag(&at("tag_link"), account.tag_link.as_deref())?;
        }
        Ok(())
    }

    /// Error if an account's dating settings, resolved through the tiers, pair a zero age
    /// cap, under which every bookmark is stale, with `cleanup_stale_to_now`, so cleanup
    /// would re-date every bookmark on every run. With no accounts, the implicit CLI/env
    /// account is checked.
    fn check_stale_to_now_cap<T: Account>(&self, accounts: &[T]) -> Result<()> {
        let source = T::SOURCE;
        let check = |account: Option<&T>, scope: String| {
            crate::settings::resolve_dates(&crate::cli::DateFlags::default(), account, self)
                .check_redating(&format!("config: {scope}"), source)
        };
        if accounts.is_empty() {
            return check(None, format!("{source} (implicit CLI/env account)"));
        }
        for (index, account) in accounts.iter().enumerate() {
            check(Some(account), format!("{source}[{index}]"))?;
        }
        Ok(())
    }
}

/// Where and what a TOML error is, never the source text around it. toml's own
/// `Display` quotes the offending line, which may hold an inline `token` or `cookie`.
/// Without a span it prints only the message and key path, so that form is safe.
fn describe_toml_error(error: &toml::de::Error, text: &str) -> String {
    let message = error.message();
    let Some(span) = error.span() else {
        return error
            .to_string()
            .replacen(message, &without_unexpected_value(message), 1)
            .trim_end()
            .replace('\n', "; ");
    };
    let (line, column) = line_and_column(text, span.start);
    let message = without_unexpected_value(message.trim_end()).replace('\n', "; ");
    format!("line {line}, column {column}: {message}")
}

/// Drop the value serde quotes in a type mismatch (`invalid type: string "...",
/// expected usize`): a secret pasted under a non-string key would land there.
fn without_unexpected_value(message: &str) -> String {
    for prefix in ["invalid type: ", "invalid value: "] {
        let Some(rest) = message.strip_prefix(prefix) else {
            continue;
        };
        let (unexpected, expected) = rest
            .rfind(", expected ")
            .map_or((rest, ""), |at| rest.split_at(at));
        let kind = [" `", " \""]
            .iter()
            .filter_map(|quote| unexpected.find(quote))
            .min()
            .map_or(unexpected, |at| &unexpected[..at]);
        return format!("{prefix}{kind}{expected}");
    }
    message.to_owned()
}

/// The 1-based line and character column of byte `offset` in `text`.
fn line_and_column(text: &str, offset: usize) -> (usize, usize) {
    let before = &text[..text.floor_char_boundary(offset)];
    let line_start = before.rfind('\n').map_or(0, |newline| newline + 1);
    let line = before.matches('\n').count() + 1;
    let column = before[line_start..].chars().count() + 1;
    (line, column)
}

/// Error if a tag or prefix holds whitespace or a comma, which Pinboard splits on or
/// forbids. The error names `setting`, never the value, which may be a credential pasted
/// into the wrong key.
pub(crate) fn check_taggable(setting: &str, value: &str) -> Result<()> {
    if value.chars().any(char::is_whitespace) {
        bail!("{setting} must not contain whitespace");
    }
    if value.contains(',') {
        bail!("{setting} must not contain a comma");
    }
    Ok(())
}

/// Error if an optional tag/prefix in config `field` contains whitespace or a comma.
fn check_tag(field: &str, value: Option<&str>) -> Result<()> {
    value.map_or(Ok(()), |v| check_taggable(&format!("config: `{field}`"), v))
}

/// Error if any tag in the config list `field` contains whitespace or a comma.
fn check_tags(field: &str, values: Option<&[String]>) -> Result<()> {
    for (index, v) in values.unwrap_or_default().iter().enumerate() {
        check_taggable(&format!("config: `{field}[{index}]`"), v)?;
    }
    Ok(())
}

/// Error if two accounts' [`Account::account_name`]s [`slug`] alike (`a.b` and `a/b`,
/// `Alice` and `alice`, or none and `default`), so every `backup --all` would fail in
/// `check_stem_collisions`. `remedy` names the fields to change; the error names positions,
/// never the names.
///
/// [`slug`]: crate::backup::slug
fn check_distinct_slugs<T: Account>(accounts: &[T], remedy: &str) -> Result<()> {
    let source = T::SOURCE;
    let mut first_with_slug = HashMap::new();
    for (index, account) in accounts.iter().enumerate() {
        let slug = crate::backup::slug(account.account_name());
        let Some(first) = first_with_slug.get(&slug) else {
            first_with_slug.insert(slug, index);
            continue;
        };
        if slug == "default" {
            bail!(
                "config: {source}[{first}] and {source}[{index}] would both back up as \
                 `{source}-default`; give one of them a distinct {remedy}"
            );
        }
        bail!(
            "config: {source}[{first}] and {source}[{index}] would back up to the same \
             files, since their account names make the same file name; give one of them \
             a distinct {remedy}"
        );
    }
    Ok(())
}

/// Error if two accounts of a source share a name, which leaves the second unreachable.
fn check_unique_names<T: Account>(accounts: &[T]) -> Result<()> {
    let source = T::SOURCE;
    let mut first_with_name = HashMap::new();
    for (index, account) in accounts.iter().enumerate() {
        let Some(name) = account.account_name() else {
            continue;
        };
        if let Some(first) = first_with_name.insert(name, index) {
            bail!("config: {source}[{index}] has the same account name as {source}[{first}]");
        }
    }
    Ok(())
}

/// Error if `value` isn't a bare host as the `url` crate spells it: it is interpolated into
/// URLs, set as a host and compared with parsed hosts, so a scheme, slash, port or any form
/// the parser rewrites would corrupt or never match. A trailing `.` would defeat
/// `cleanup reddit`'s reddit.com filter.
fn check_domain(field: &str, value: Option<&str>) -> Result<()> {
    let Some(domain) = value else {
        return Ok(());
    };
    let problem = if domain.is_empty() {
        "is empty"
    } else if domain.contains('/') {
        "contains a `/`"
    } else if domain.chars().any(char::is_whitespace) {
        "contains whitespace"
    } else if domain.contains(':') && !domain.starts_with('[') {
        "contains a `:` (a port is not allowed)"
    } else if domain.ends_with('.') {
        "ends with a `.`"
    } else if !domain.is_ascii() {
        "is not ASCII (write a non-ASCII name in its punycode `xn--` form)"
    } else if !url::Host::parse(domain)
        .is_ok_and(|host| host.to_string().eq_ignore_ascii_case(domain))
    {
        "is not a valid host"
    } else {
        return Ok(());
    };
    bail!("config: `{field}` {problem}; it must be a bare host like \"old.reddit.com\"")
}

/// The account's [`RedditConfig`], each field defaulting.
impl From<&RedditAccount> for RedditConfig {
    fn from(account: &RedditAccount) -> Self {
        let d = RedditConfig::default();
        RedditConfig {
            domain: account.reddit_domain.clone().unwrap_or(d.domain),
            tags: account.tags.clone().unwrap_or(d.tags),
            subreddit_prefix: account
                .tag_subreddit_prefix
                .clone()
                .unwrap_or(d.subreddit_prefix),
            comment: account.tag_comment.clone().unwrap_or(d.comment),
            nsfw: account.tag_nsfw.clone().unwrap_or(d.nsfw),
            author_prefix: account.tag_author_prefix.clone().unwrap_or(d.author_prefix),
            flair_prefix: account.tag_flair_prefix.clone().unwrap_or(d.flair_prefix),
            media_prefix: account.tag_media_prefix.clone().unwrap_or(d.media_prefix),
            media_types: account.tag_media_types.clone().unwrap_or(d.media_types),
        }
    }
}

/// One GitHub account: a token and the tags.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitHubAccount {
    pub name: Option<String>,
    pub token: Option<String>,
    pub token_file: Option<String>,
    pub on_auth_failure: Option<String>,
    pub limit: Option<SyncLimit>,
    /// Override `[pinboard].toread` for this account's new bookmarks.
    pub toread: Option<bool>,
    /// Override `[pinboard].public` for this account's new bookmarks.
    pub public: Option<bool>,
    /// Override `use_post_date` for this account.
    pub use_post_date: Option<bool>,
    /// Override `post_date_max_age_days` for this account.
    pub post_date_max_age_days: Option<u64>,
    /// Override `cleanup_stale_to_now` for this account.
    pub cleanup_stale_to_now: Option<bool>,
    pub tag_lang_prefix: Option<String>,
    pub tag_author_prefix: Option<String>,
    /// Tags applied to every bookmark (default `["github-star"]`); a full override.
    pub tags: Option<Vec<String>>,
    /// Whether to sync the account's starred gists (default on).
    pub gists: Option<bool>,
    /// Tags applied to every gist (default `tags` plus `github-gist`); a full override.
    pub gist_tags: Option<Vec<String>>,
    pub gist_tag_lang_prefix: Option<String>,
    pub gist_tag_author_prefix: Option<String>,
    /// Override `[defaults.github]`'s `gist_post_date` for this account.
    pub gist_post_date: Option<GistPostDate>,
}

impl GitHubConfig {
    /// The [`GitHubConfig`] of `account`, or the implicit CLI/env account: each field
    /// defaulting, a gist tag field to the stars', and `gist_post_date` to
    /// `[defaults.github]`, then `now`.
    pub fn resolve(account: Option<&GitHubAccount>, defaults: &GitHubDefaults) -> Self {
        let d = GitHubConfig::default();
        let field = |pick: fn(&GitHubAccount) -> Option<&String>, default: String| {
            account.and_then(pick).cloned().unwrap_or(default)
        };
        let tags = account.and_then(|a| a.tags.clone()).unwrap_or(d.tags);
        let lang_prefix = field(|a| a.tag_lang_prefix.as_ref(), d.lang_prefix);
        let author_prefix = field(|a| a.tag_author_prefix.as_ref(), d.author_prefix);
        let inherited = GistConfig::inheriting(&tags, &lang_prefix, &author_prefix);
        let gists = GistConfig {
            enabled: account.and_then(|a| a.gists).unwrap_or(inherited.enabled),
            tags: account
                .and_then(|a| a.gist_tags.clone())
                .unwrap_or(inherited.tags),
            lang_prefix: field(|a| a.gist_tag_lang_prefix.as_ref(), inherited.lang_prefix),
            author_prefix: field(
                |a| a.gist_tag_author_prefix.as_ref(),
                inherited.author_prefix,
            ),
            post_date: crate::settings::resolve_setting(
                None,
                account.and_then(|a| a.gist_post_date),
                defaults.gist_post_date,
                GistPostDate::default(),
            )
            .value,
        };
        GitHubConfig {
            tags,
            lang_prefix,
            author_prefix,
            gists,
        }
    }
}

/// One HackerNews account: a public username and the tags.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HackerNewsAccount {
    pub name: Option<String>,
    pub username: Option<String>,
    pub limit: Option<SyncLimit>,
    /// Override `[pinboard].toread` for this account's new bookmarks.
    pub toread: Option<bool>,
    /// Override `[pinboard].public` for this account's new bookmarks.
    pub public: Option<bool>,
    /// Override `use_post_date` for this account.
    pub use_post_date: Option<bool>,
    /// Override `post_date_max_age_days` for this account.
    pub post_date_max_age_days: Option<u64>,
    /// Override `cleanup_stale_to_now` for this account.
    pub cleanup_stale_to_now: Option<bool>,
    pub tag_comment: Option<String>,
    pub tag_author_prefix: Option<String>,
    pub tag_special_prefix: Option<String>,
    pub tag_link: Option<String>,
    /// Tags applied to every bookmark (default `["hackernews"]`); a full override.
    pub tags: Option<Vec<String>>,
}

/// The account's [`HackerNewsConfig`], each field defaulting.
impl From<&HackerNewsAccount> for HackerNewsConfig {
    fn from(account: &HackerNewsAccount) -> Self {
        let d = HackerNewsConfig::default();
        HackerNewsConfig {
            tags: account.tags.clone().unwrap_or(d.tags),
            comment: account.tag_comment.clone().unwrap_or(d.comment),
            author_prefix: account.tag_author_prefix.clone().unwrap_or(d.author_prefix),
            special_prefix: account
                .tag_special_prefix
                .clone()
                .unwrap_or(d.special_prefix),
            link_tag: account.tag_link.clone().unwrap_or(d.link_tag),
        }
    }
}

/// Shows whether an inline secret is set, never its value. The `Debug` impls below
/// destructure every field so a new one cannot be added without deciding how it shows.
struct Redacted<'a>(Option<&'a str>);

impl fmt::Debug for Redacted<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            Some(_) => f.write_str("Some(<redacted>)"),
            None => f.write_str("None"),
        }
    }
}

impl fmt::Debug for PinboardConfig {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            token,
            token_file,
            public,
            toread,
            rate_limit_secs,
            limit,
            use_post_date,
            post_date_max_age_days,
            cleanup_stale_to_now,
        } = self;
        f.debug_struct("PinboardConfig")
            .field("token", &Redacted(token.as_deref()))
            .field("token_file", token_file)
            .field("public", public)
            .field("toread", toread)
            .field("rate_limit_secs", rate_limit_secs)
            .field("limit", limit)
            .field("use_post_date", use_post_date)
            .field("post_date_max_age_days", post_date_max_age_days)
            .field("cleanup_stale_to_now", cleanup_stale_to_now)
            .finish()
    }
}

impl fmt::Debug for RedditAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            name,
            username,
            cookie,
            cookie_file,
            on_auth_failure,
            limit,
            toread,
            public,
            use_post_date,
            post_date_max_age_days,
            cleanup_stale_to_now,
            reddit_domain,
            tag_subreddit_prefix,
            tag_comment,
            tag_nsfw,
            tag_author_prefix,
            tag_flair_prefix,
            tag_media_prefix,
            tag_media_types,
            tags,
        } = self;
        f.debug_struct("RedditAccount")
            .field("name", name)
            .field("username", username)
            .field("cookie", &Redacted(cookie.as_deref()))
            .field("cookie_file", cookie_file)
            .field("on_auth_failure", on_auth_failure)
            .field("limit", limit)
            .field("toread", toread)
            .field("public", public)
            .field("use_post_date", use_post_date)
            .field("post_date_max_age_days", post_date_max_age_days)
            .field("cleanup_stale_to_now", cleanup_stale_to_now)
            .field("reddit_domain", reddit_domain)
            .field("tag_subreddit_prefix", tag_subreddit_prefix)
            .field("tag_comment", tag_comment)
            .field("tag_nsfw", tag_nsfw)
            .field("tag_author_prefix", tag_author_prefix)
            .field("tag_flair_prefix", tag_flair_prefix)
            .field("tag_media_prefix", tag_media_prefix)
            .field("tag_media_types", tag_media_types)
            .field("tags", tags)
            .finish()
    }
}

impl fmt::Debug for GitHubAccount {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let Self {
            name,
            token,
            token_file,
            on_auth_failure,
            limit,
            toread,
            public,
            use_post_date,
            post_date_max_age_days,
            cleanup_stale_to_now,
            tag_lang_prefix,
            tag_author_prefix,
            tags,
            gists,
            gist_tags,
            gist_tag_lang_prefix,
            gist_tag_author_prefix,
            gist_post_date,
        } = self;
        f.debug_struct("GitHubAccount")
            .field("name", name)
            .field("token", &Redacted(token.as_deref()))
            .field("token_file", token_file)
            .field("on_auth_failure", on_auth_failure)
            .field("limit", limit)
            .field("toread", toread)
            .field("public", public)
            .field("use_post_date", use_post_date)
            .field("post_date_max_age_days", post_date_max_age_days)
            .field("cleanup_stale_to_now", cleanup_stale_to_now)
            .field("tag_lang_prefix", tag_lang_prefix)
            .field("tag_author_prefix", tag_author_prefix)
            .field("tags", tags)
            .field("gists", gists)
            .field("gist_tags", gist_tags)
            .field("gist_tag_lang_prefix", gist_tag_lang_prefix)
            .field("gist_tag_author_prefix", gist_tag_author_prefix)
            .field("gist_post_date", gist_post_date)
            .finish()
    }
}

/// One source's account type: where its accounts and defaults live, and the settings every
/// source shares (`max_age_days` and `stale_to_now` for the `post_date_max_age_days` and
/// `cleanup_stale_to_now` keys), each `None` to fall through to the next tier.
pub trait Account: Sized {
    /// The source's name in messages, job labels, backup file stems and `doctor` lines.
    const SOURCE: &'static str;

    /// The name the account is selected by (`sync <source> <name>`): its `name`, else its
    /// `username` where it has one.
    fn account_name(&self) -> Option<&str>;

    /// The source's configured accounts (`[[reddit]]` and so on).
    fn configured(config: &Config) -> &[Self];

    /// The source's `[defaults.<source>]` tier.
    fn defaults(config: &Config) -> &SourceDefaults;

    fn limit(&self) -> Option<SyncLimit>;
    fn toread(&self) -> Option<bool>;
    fn public(&self) -> Option<bool>;
    fn use_post_date(&self) -> Option<bool>;
    fn max_age_days(&self) -> Option<u64>;
    fn stale_to_now(&self) -> Option<bool>;
}

impl Account for RedditAccount {
    const SOURCE: &'static str = "reddit";

    fn account_name(&self) -> Option<&str> {
        self.name.as_deref().or(self.username.as_deref())
    }

    fn configured(config: &Config) -> &[Self] {
        &config.reddit
    }

    fn defaults(config: &Config) -> &SourceDefaults {
        &config.defaults.reddit
    }

    fn limit(&self) -> Option<SyncLimit> {
        self.limit
    }

    fn toread(&self) -> Option<bool> {
        self.toread
    }

    fn public(&self) -> Option<bool> {
        self.public
    }

    fn use_post_date(&self) -> Option<bool> {
        self.use_post_date
    }

    fn max_age_days(&self) -> Option<u64> {
        self.post_date_max_age_days
    }

    fn stale_to_now(&self) -> Option<bool> {
        self.cleanup_stale_to_now
    }
}

impl Account for GitHubAccount {
    const SOURCE: &'static str = "github";

    fn account_name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    fn configured(config: &Config) -> &[Self] {
        &config.github
    }

    fn defaults(config: &Config) -> &SourceDefaults {
        &config.defaults.github.common
    }

    fn limit(&self) -> Option<SyncLimit> {
        self.limit
    }

    fn toread(&self) -> Option<bool> {
        self.toread
    }

    fn public(&self) -> Option<bool> {
        self.public
    }

    fn use_post_date(&self) -> Option<bool> {
        self.use_post_date
    }

    fn max_age_days(&self) -> Option<u64> {
        self.post_date_max_age_days
    }

    fn stale_to_now(&self) -> Option<bool> {
        self.cleanup_stale_to_now
    }
}

impl Account for HackerNewsAccount {
    const SOURCE: &'static str = "hackernews";

    fn account_name(&self) -> Option<&str> {
        self.name.as_deref().or(self.username.as_deref())
    }

    fn configured(config: &Config) -> &[Self] {
        &config.hackernews
    }

    fn defaults(config: &Config) -> &SourceDefaults {
        &config.defaults.hackernews
    }

    fn limit(&self) -> Option<SyncLimit> {
        self.limit
    }

    fn toread(&self) -> Option<bool> {
        self.toread
    }

    fn public(&self) -> Option<bool> {
        self.public
    }

    fn use_post_date(&self) -> Option<bool> {
        self.use_post_date
    }

    fn max_age_days(&self) -> Option<u64> {
        self.post_date_max_age_days
    }

    fn stale_to_now(&self) -> Option<bool> {
        self.cleanup_stale_to_now
    }
}

/// `own`, then each of `configured`: the order a source checks its accounts in when it
/// recognises a bookmark another of them wrote, its own account first.
pub fn own_then_configured<'a, C>(
    own: &'a C,
    configured: &'a [C],
) -> impl Iterator<Item = &'a C> + 'a {
    std::iter::once(own).chain(configured)
}

/// The account `name`, or the first; `Ok(None)` with no name and no accounts, for the
/// implicit CLI/env one.
pub fn select_account<'a, T: Account>(
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
mod tests;
