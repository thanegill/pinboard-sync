//! The `--config <path>` TOML: one Pinboard destination, an optional auth-failure
//! hook, and per-source arrays of accounts. Each account holds non-secret settings
//! (tags, domain, limit) plus secret values inline or as a `*_file` path. Secrets
//! resolve through the ladder in `main`; this module is the typed schema + the
//! account → typed-config mapping. Every field is optional, so an absent or empty
//! config yields all defaults.

use std::collections::HashMap;
use std::fmt;
use std::num::NonZeroUsize;
use std::str::FromStr;

use anyhow::{anyhow, bail, Result};
use serde::Deserialize;

use crate::github::GitHubConfig;
use crate::hackernews::HackerNewsConfig;
use crate::model::RedditConfig;

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
    /// Per-source default overrides (the middle tier between `[pinboard]`/`[hooks]`
    /// globals and a per-account override).
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub reddit: Vec<RedditAccount>,
    #[serde(default)]
    pub github: Vec<GitHubAccount>,
    #[serde(default)]
    pub hackernews: Vec<HackerNewsAccount>,
}

/// Per-source default overrides, keyed by source (`[defaults.reddit]` etc.). Each
/// field, when set, overrides the corresponding global default and is in turn
/// overridden by a per-account value of the same name.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Defaults {
    #[serde(default)]
    pub reddit: SourceDefaults,
    #[serde(default)]
    pub github: SourceDefaults,
    #[serde(default)]
    pub hackernews: SourceDefaults,
}

/// The overridable settings, as a per-source default tier. All `Option` — `None`
/// falls through to the global default.
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

/// `backup` output settings. The directory lives in the config because the NixOS timer
/// runs a bare `backup --all` with nowhere to put a flag.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BackupConfig {
    /// Directory the snapshot is written into, overwritten in place each run. Unset means
    /// `--out` is required.
    pub directory: Option<String>,
}

/// Cross-cutting hooks.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hooks {
    /// Shell command run on a re-auth failure (per-account override available).
    pub on_auth_failure: Option<String>,
}

/// The shared Pinboard destination.
#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinboardConfig {
    pub token: Option<String>,
    pub token_file: Option<String>,
    /// Write bookmarks public (default private).
    #[serde(default)]
    pub public: bool,
    /// Mark new bookmarks to-read/unread (default false). Per-account overridable.
    #[serde(default)]
    pub toread: bool,
    /// Seconds to pause between `posts/add` writes (default 3, what Pinboard asks for).
    pub rate_limit_secs: Option<u64>,
    /// Global [`SyncLimit`] (per-source/per-account overridable; unset = `auto`). The CLI
    /// `--limit` still wins.
    pub limit: Option<SyncLimit>,
    /// Set a bookmark's creation date to the source post date (default false).
    /// Per-source/per-account overridable.
    #[serde(default)]
    pub use_post_date: bool,
    /// With `use_post_date`, only backdate posts at most this many days old (unset =
    /// [`DEFAULT_MAX_AGE_DAYS`]); older posts use "now" (sync) / keep their existing
    /// time (cleanup).
    pub post_date_max_age_days: Option<u64>,
    /// With `use_post_date`, whether `cleanup` re-dates posts older than the cap to "now"
    /// instead of leaving their existing date (default false — keeps cleanup idempotent).
    #[serde(default)]
    pub cleanup_stale_to_now: bool,
}

/// Built-in default backdate window (days) when no config sets `post_date_max_age_days`.
pub const DEFAULT_MAX_AGE_DAYS: u64 = 30;

/// How much one `sync` run writes and reads: the `limit` config key at every tier and
/// the `--limit` flag. Written `auto`, `all`, or a positive number. `0`, which used to
/// mean "no cap", is rejected rather than quietly mapped to either word.
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

/// The complaint about a malformed `limit`. It names the field and never the value, which
/// could be a credential pasted into the wrong key.
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

/// One reddit account: whose saves to read, the session cookie, and the
/// (non-secret) tag/domain vocabulary.
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
        let config: Config = toml::from_str(text)
            .map_err(|e| anyhow!("parsing config: {}", describe_toml_error(&e, text)))?;
        config.validate()?;
        Ok(config)
    }

    /// Reject tag values containing whitespace — Pinboard tags can't contain spaces
    /// (its API splits the tag string on them), so a space here is silently
    /// corrupting and should fail loudly instead.
    fn validate(&self) -> Result<()> {
        if self.backup.directory.as_deref().is_some_and(str::is_empty) {
            bail!("backup.directory must not be empty");
        }
        check_unique_names("reddit", &self.reddit)?;
        check_unique_names("github", &self.github)?;
        check_unique_names("hackernews", &self.hackernews)?;

        for (index, account) in self.reddit.iter().enumerate() {
            let at = |field: &str| format!("reddit[{index}].{field}");
            check_domain(&at("reddit_domain"), &account.reddit_domain)?;
            check_tags(&at("tags"), &account.tags)?;
            check_tag(&at("tag_subreddit_prefix"), &account.tag_subreddit_prefix)?;
            check_tag(&at("tag_comment"), &account.tag_comment)?;
            check_tag(&at("tag_nsfw"), &account.tag_nsfw)?;
            check_tag(&at("tag_author_prefix"), &account.tag_author_prefix)?;
            check_tag(&at("tag_flair_prefix"), &account.tag_flair_prefix)?;
            check_tag(&at("tag_media_prefix"), &account.tag_media_prefix)?;
        }
        for (index, account) in self.github.iter().enumerate() {
            let at = |field: &str| format!("github[{index}].{field}");
            check_tags(&at("tags"), &account.tags)?;
            check_tag(&at("tag_lang_prefix"), &account.tag_lang_prefix)?;
        }
        for (index, account) in self.hackernews.iter().enumerate() {
            let at = |field: &str| format!("hackernews[{index}].{field}");
            check_tags(&at("tags"), &account.tags)?;
            check_tag(&at("tag_comment"), &account.tag_comment)?;
            check_tag(&at("tag_author_prefix"), &account.tag_author_prefix)?;
            check_tag(&at("tag_special_prefix"), &account.tag_special_prefix)?;
            check_tag(&at("tag_link"), &account.tag_link)?;
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

/// Error if `value` (a tag or tag prefix) contains whitespace — Pinboard tags can't
/// contain spaces (its API splits the tag string on them), so a space here silently
/// corrupts and should fail loudly instead. The error names the field, never the value,
/// which may be a credential pasted into the wrong key.
pub(crate) fn reject_whitespace(field: &str, value: &str) -> Result<()> {
    if value.chars().any(char::is_whitespace) {
        bail!("config: `{field}` must not contain whitespace");
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
    for (index, v) in values.iter().flatten().enumerate() {
        reject_whitespace(&format!("{field}[{index}]"), v)?;
    }
    Ok(())
}

/// Error if two accounts of a source share a `name` (the second would be
/// unreachable by name in `sync <source> <name>`).
fn check_unique_names<T: Named>(source: &str, accounts: &[T]) -> Result<()> {
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

/// Error if `value` isn't a bare host (it's interpolated as `https://<domain>…`, so
/// a scheme, slash, or whitespace would corrupt the bookmark URLs).
fn check_domain(field: &str, value: &Option<String>) -> Result<()> {
    let Some(domain) = value else {
        return Ok(());
    };
    let problem = if domain.is_empty() {
        "is empty"
    } else if domain.contains('/') {
        "contains a `/`"
    } else if domain.chars().any(char::is_whitespace) {
        "contains whitespace"
    } else {
        return Ok(());
    };
    bail!("config: `{field}` {problem}; it must be a bare host like \"old.reddit.com\"")
}

/// The account's non-secret [`RedditConfig`] (bookmark domain + tag vocabulary), each
/// field falling back to its built-in default.
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

/// One GitHub account: a personal access token plus the (non-secret) tag config.
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
    /// Tags applied to every bookmark (default `["github-star"]`); a full override.
    pub tags: Option<Vec<String>>,
}

/// The account's non-secret [`GitHubConfig`] (tag vocabulary), each field falling back
/// to its built-in default.
impl From<&GitHubAccount> for GitHubConfig {
    fn from(account: &GitHubAccount) -> Self {
        let d = GitHubConfig::default();
        GitHubConfig {
            tags: account.tags.clone().unwrap_or(d.tags),
            lang_prefix: account.tag_lang_prefix.clone().unwrap_or(d.lang_prefix),
        }
    }
}

/// One HackerNews account: a public username plus the (non-secret) tag config.
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

/// The account's non-secret [`HackerNewsConfig`] (tag vocabulary), each field falling
/// back to its built-in default.
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
struct Redacted<'a>(&'a Option<String>);

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
            .field("token", &Redacted(token))
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
            .field("cookie", &Redacted(cookie))
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
            tags,
        } = self;
        f.debug_struct("GitHubAccount")
            .field("name", name)
            .field("token", &Redacted(token))
            .field("token_file", token_file)
            .field("on_auth_failure", on_auth_failure)
            .field("limit", limit)
            .field("toread", toread)
            .field("public", public)
            .field("use_post_date", use_post_date)
            .field("post_date_max_age_days", post_date_max_age_days)
            .field("cleanup_stale_to_now", cleanup_stale_to_now)
            .field("tag_lang_prefix", tag_lang_prefix)
            .field("tags", tags)
            .finish()
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
impl_named!(RedditAccount => username, GitHubAccount, HackerNewsAccount => username);

/// The per-account settings shared across every source. Each returns the
/// account-level override, or `None` to fall through to the per-source default and
/// then the `[pinboard]` global — letting the job/cleanup builders resolve them
/// generically (see `resolve_setting`/`resolve_dates` in `main`). The method names
/// normalize the two longer field names (`post_date_max_age_days`,
/// `cleanup_stale_to_now`).
pub trait Account: Named {
    fn limit(&self) -> Option<SyncLimit>;
    fn toread(&self) -> Option<bool>;
    fn public(&self) -> Option<bool>;
    fn use_post_date(&self) -> Option<bool>;
    fn max_age_days(&self) -> Option<u64>;
    fn stale_to_now(&self) -> Option<bool>;
}

/// Implement [`Account`] by reading the identically-purposed fields each account
/// struct carries (all `Copy` `Option`s).
macro_rules! impl_account {
    ($($t:ty),+ $(,)?) => {
        $(impl Account for $t {
            fn limit(&self) -> Option<SyncLimit> { self.limit }
            fn toread(&self) -> Option<bool> { self.toread }
            fn public(&self) -> Option<bool> { self.public }
            fn use_post_date(&self) -> Option<bool> { self.use_post_date }
            fn max_age_days(&self) -> Option<u64> { self.post_date_max_age_days }
            fn stale_to_now(&self) -> Option<bool> { self.cleanup_stale_to_now }
        })+
    };
}
impl_account!(RedditAccount, GitHubAccount, HackerNewsAccount);

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
        assert!(cfg.pinboard.limit.is_none());
        assert!(cfg.defaults.reddit.toread.is_none());
    }

    fn assert_error_hides(text: &str, secret: &str, location: &str) {
        let err = Config::parse(text).expect_err("malformed config must not parse");
        for rendered in [format!("{err}"), format!("{err:#}"), format!("{err:?}")] {
            assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
            assert!(rendered.contains(location), "no {location} in: {rendered}");
        }
    }

    #[test]
    fn syntax_error_on_an_inline_secret_reports_position_not_the_line() {
        assert_error_hides(
            "[hooks]\n\n[pinboard]\ntoken = \"user:SECRET123\ntoread = true\n",
            "SECRET123",
            "line 4, column",
        );
        assert_error_hides(
            "[[reddit]]\nname = \"main\"\ncookie = reddit_session=SECRET456\n",
            "SECRET456",
            "line 3, column",
        );
    }

    #[test]
    fn type_error_on_an_inline_secret_reports_position_not_the_line() {
        assert_error_hides(
            "[pinboard]\ntoken = [\"SECRET789\"]\n",
            "SECRET789",
            "line 2, column 9",
        );
    }

    #[test]
    fn limit_takes_auto_all_or_a_positive_count() {
        let limit = |value: &str| {
            Config::parse(&format!("[pinboard]\nlimit = {value}\n"))
                .unwrap()
                .pinboard
                .limit
        };
        assert_eq!(limit("\"auto\""), Some(SyncLimit::Auto));
        assert_eq!(limit("\"all\""), Some(SyncLimit::All));
        assert_eq!(
            limit("25"),
            Some(SyncLimit::Max(NonZeroUsize::new(25).unwrap()))
        );
        assert_eq!(Config::parse("").unwrap().pinboard.limit, None);
        assert_eq!(SyncLimit::default(), SyncLimit::Auto);
    }

    #[test]
    fn a_bad_limit_names_the_field_not_the_value() {
        for (value, column) in [("0", 9), ("-3", 9), ("\"SECRET42\"", 9), ("1.5", 9)] {
            assert_error_hides(
                &format!("[[hackernews]]\nlimit = {value}\n"),
                "SECRET42",
                &format!("line 2, column {column}: `limit` must be `auto` (no cap;"),
            );
        }
        assert_error_hides(
            "[[hackernews]]\nlimit = [\"SECRET43\"]\n",
            "SECRET43",
            "line 2, column 9: invalid type: sequence, expected `limit` as `auto`",
        );
        assert_error_hides(
            "[[hackernews]]\nlimit = true\n",
            "`true`",
            "line 2, column 9: invalid type: boolean, expected `limit` as `auto`",
        );
    }

    #[test]
    fn secret_under_a_non_string_key_is_not_quoted_back() {
        assert_error_hides(
            "[pinboard]\nrate_limit_secs = \"user:SECRET321\"\n",
            "SECRET321",
            "line 2, column 19: invalid type: string, expected u64",
        );
        assert_error_hides(
            "[[reddit]]\ntoread = \"reddit_session=SECRET654, expected x\"\n",
            "SECRET654",
            "line 2, column 10: invalid type: string, expected a boolean",
        );
        assert_error_hides(
            "[[github]]\ntag_lang_prefix = 5\n",
            "`5`",
            "invalid type: integer, expected a string",
        );
    }

    #[test]
    fn validation_error_names_the_field_not_the_value() {
        assert_error_hides(
            "[[reddit]]\n[[reddit]]\ntags = [\"reddit\", \"reddit_session=SECRET1 x\"]\n",
            "SECRET1",
            "`reddit[1].tags[1]` must not contain whitespace",
        );
        assert_error_hides(
            "[[github]]\ntag_lang_prefix = \"ghp SECRET2\"\n",
            "SECRET2",
            "`github[0].tag_lang_prefix` must not contain whitespace",
        );
        assert_error_hides(
            "[[hackernews]]\ntag_link = \"x SECRET3\"\n",
            "SECRET3",
            "`hackernews[0].tag_link` must not contain whitespace",
        );
        assert_error_hides(
            "[[reddit]]\nreddit_domain = \"user:SECRET4/x\"\n",
            "SECRET4",
            "`reddit[0].reddit_domain` contains a `/`",
        );
        assert_error_hides(
            "[[reddit]]\nreddit_domain = \"reddit_session=SECRET5 x\"\n",
            "SECRET5",
            "`reddit[0].reddit_domain` contains whitespace",
        );
        assert_error_hides(
            "[[reddit]]\nreddit_domain = \"\"\n",
            "SECRET",
            "`reddit[0].reddit_domain` is empty",
        );
        assert_error_hides(
            "[[github]]\nname = \"SECRET6\"\n[[github]]\n[[github]]\nname = \"SECRET6\"\n",
            "SECRET6",
            "github[2] has the same account name as github[0]",
        );
    }

    #[test]
    fn debug_of_a_parsed_config_redacts_inline_secrets() {
        let cfg = Config::parse(
            "[pinboard]\ntoken = \"user:PINSECRET\"\n\
             [[reddit]]\ncookie = \"reddit_session=REDDITSECRET\"\ncookie_file = \"/run/cookie\"\n\
             [[reddit]]\n\
             [[github]]\ntoken = \"ghp_GITHUBSECRET\"\n",
        )
        .unwrap();
        for rendered in [format!("{cfg:?}"), format!("{cfg:#?}")] {
            for secret in ["PINSECRET", "REDDITSECRET", "GITHUBSECRET"] {
                assert!(!rendered.contains(secret), "leaked {secret}: {rendered}");
            }
            assert!(rendered.contains("/run/cookie"), "{rendered}");
        }
        assert_eq!(
            format!("{:?}", Redacted(&cfg.pinboard.token)),
            "Some(<redacted>)"
        );
        assert_eq!(format!("{:?}", Redacted(&cfg.reddit[1].cookie)), "None");
        assert!(format!("{:?}", cfg.reddit[0]).contains("cookie: Some(<redacted>)"));
        assert!(format!("{:?}", cfg.reddit[1]).contains("cookie: None"));
        assert!(format!("{:?}", cfg.github[0]).contains("token: Some(<redacted>)"));
    }

    #[test]
    fn spanless_toml_error_still_drops_the_value() {
        let error = <toml::de::Error as serde::de::Error>::custom(
            "invalid type: string \"SECRET987\", expected usize",
        );
        assert_eq!(
            describe_toml_error(&error, ""),
            "invalid type: string, expected usize"
        );
    }

    #[test]
    fn toml_error_column_counts_characters() {
        assert_eq!(line_and_column("a = \"é\"\nb", 9), (2, 1));
        assert_eq!(line_and_column("ab", 1), (1, 2));
        assert_eq!(line_and_column("é", 1), (1, 1));
        assert_eq!(line_and_column("ab", 99), (1, 3));
    }

    #[test]
    fn parses_per_source_defaults_and_account_overrides() {
        let cfg = Config::parse(
            r#"
            [pinboard]
            toread = true

            [defaults.reddit]
            toread = false
            limit = 7

            [[reddit]]
            name = "main"
            toread = true

            [[reddit]]
            name = "alt"
            "#,
        )
        .unwrap();

        assert!(cfg.pinboard.toread);
        assert_eq!(cfg.defaults.reddit.toread, Some(false));
        assert_eq!(
            cfg.defaults.reddit.limit,
            Some(SyncLimit::Max(NonZeroUsize::new(7).unwrap()))
        );
        assert_eq!(cfg.reddit[0].toread, Some(true));
        assert_eq!(cfg.reddit[1].toread, None);
        // An unknown key inside the per-source table is still rejected.
        assert!(Config::parse("[defaults.reddit]\nnonsense = 1").is_err());
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
        let rc = RedditConfig::from(main);
        assert_eq!(rc.domain, "www.reddit.com");
        assert_eq!(rc.tags, vec!["reddit", "account:main"]);
        assert_eq!(rc.media_types, vec!["image"]);
        // Unset tag fields keep their defaults; an unset `tags` defaults to ["reddit"].
        assert_eq!(rc.subreddit_prefix, "subreddit:");
        assert_eq!(RedditConfig::from(&cfg.reddit[1]).tags, vec!["reddit"]);
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
        let pinboard = documented_fields!(PinboardConfig {
            token,
            token_file,
            public,
            toread,
            rate_limit_secs,
            limit,
            use_post_date,
            post_date_max_age_days,
            cleanup_stale_to_now,
        });
        // Note this list does not destructure `Config` itself, so a *new top-level table*
        // (like `[backup]`) has to be added here deliberately — only its fields are
        // compile-checked, not its presence.
        let backup = documented_fields!(BackupConfig { directory });
        // The per-source defaults tier (`[defaults.<source>]`).
        let source_defaults = documented_fields!(SourceDefaults {
            toread,
            public,
            limit,
            on_auth_failure,
            use_post_date,
            post_date_max_age_days,
            cleanup_stale_to_now,
        });
        let reddit = documented_fields!(RedditAccount {
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
        });
        let github = documented_fields!(GitHubAccount {
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
            tags,
        });
        let hackernews = documented_fields!(HackerNewsAccount {
            name,
            username,
            limit,
            toread,
            public,
            use_post_date,
            post_date_max_age_days,
            cleanup_stale_to_now,
            tag_comment,
            tag_author_prefix,
            tag_special_prefix,
            tag_link,
            tags,
        });

        for &key in hooks
            .iter()
            .chain(&pinboard)
            .chain(&backup)
            .chain(&source_defaults)
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
            GitHubAccount {
                token: Some("ghp_x".into()),
                ..Default::default()
            }
            .account_name(),
            None
        );
    }
}
