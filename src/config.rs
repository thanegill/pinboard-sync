//! The `--config <path>` TOML: one Pinboard destination, an optional auth-failure
//! hook, and per-source arrays of accounts. Each account holds non-secret settings
//! (tags, domain, limit) plus secret values inline or as a `*_file` path. Secrets
//! resolve through the ladder in `secrets`; this module is the typed schema + the
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

    /// Parse a config from TOML text, validating tag fields.
    pub fn parse(text: &str) -> Result<Self> {
        let config: Config = toml::from_str(text)
            .map_err(|e| anyhow!("parsing config: {}", describe_toml_error(&e, text)))?;
        config.validate()?;
        Ok(config)
    }

    /// Reject tag values containing whitespace or commas — Pinboard tags can't contain
    /// either (its API splits the tag string on spaces), so one here is silently
    /// corrupting and should fail loudly instead.
    fn validate(&self) -> Result<()> {
        if self.backup.directory.as_deref().is_some_and(str::is_empty) {
            bail!("backup.directory must not be empty");
        }
        check_unique_names("reddit", &self.reddit)?;
        check_unique_names("github", &self.github)?;
        check_one_unnamed("github", &self.github)?;
        check_unique_names("hackernews", &self.hackernews)?;
        self.check_stale_to_now_cap("reddit", &self.reddit, &self.defaults.reddit)?;
        self.check_stale_to_now_cap("github", &self.github, &self.defaults.github)?;
        self.check_stale_to_now_cap("hackernews", &self.hackernews, &self.defaults.hackernews)?;

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

    /// Error if an account's dating settings, resolved through the config tiers, pair a
    /// zero age cap with `cleanup_stale_to_now`: every date is then outside the cap, so
    /// cleanup would re-date each stale bookmark on every run. With no accounts the
    /// source's implicit CLI/env account is checked instead.
    fn check_stale_to_now_cap<T: Account>(
        &self,
        source: &str,
        accounts: &[T],
        defaults: &SourceDefaults,
    ) -> Result<()> {
        let redates_every_run = |account: Option<&T>| {
            crate::timefmt::resolve_dates(
                &crate::timefmt::DateOverrides::default(),
                account,
                defaults,
                self,
            )
            .redates_every_run()
        };
        let offender = match accounts {
            [] => redates_every_run(None).then(|| format!("{source} (implicit CLI/env account)")),
            _ => accounts
                .iter()
                .position(|account| redates_every_run(Some(account)))
                .map(|index| format!("{source}[{index}]")),
        };
        match offender {
            Some(scope) => bail!(
                "config: {scope} resolves `post_date_max_age_days` to 0 with \
                 `cleanup_stale_to_now` on, so cleanup would re-date its stale bookmarks on \
                 every run; raise the cap or turn `cleanup_stale_to_now` off"
            ),
            None => Ok(()),
        }
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

/// Error if `value` (a tag or tag prefix) contains whitespace or a comma — Pinboard tags
/// can't contain either (its API splits the tag string on spaces, and its docs forbid
/// commas), so one here silently corrupts and should fail loudly instead. The error names
/// the field, never the value, which may be a credential pasted into the wrong key.
pub(crate) fn reject_untaggable(field: &str, value: &str) -> Result<()> {
    if value.chars().any(char::is_whitespace) {
        bail!("config: `{field}` must not contain whitespace");
    }
    if value.contains(',') {
        bail!("config: `{field}` must not contain a comma");
    }
    Ok(())
}

/// Error if an optional tag/prefix contains whitespace or a comma.
fn check_tag(field: &str, value: &Option<String>) -> Result<()> {
    value
        .as_deref()
        .map_or(Ok(()), |v| reject_untaggable(field, v))
}

/// Error if any tag in the list contains whitespace or a comma.
fn check_tags(field: &str, values: &Option<Vec<String>>) -> Result<()> {
    for (index, v) in values.iter().flatten().enumerate() {
        reject_untaggable(&format!("{field}[{index}]"), v)?;
    }
    Ok(())
}

/// Error if two accounts of a source would both write the `source-default` backup files,
/// so that every `backup --all` fails: each has no `name`, or one that [`slug`]s to
/// `default` (empty, `default` itself, or only punctuation).
///
/// [`slug`]: crate::backup::slug
fn check_one_unnamed<T: Named>(source: &str, accounts: &[T]) -> Result<()> {
    let mut unnamed = accounts
        .iter()
        .enumerate()
        .filter(|(_, account)| crate::backup::slug(account.account_name()) == "default")
        .map(|(index, _)| index);
    if let (Some(first), Some(second)) = (unnamed.next(), unnamed.next()) {
        bail!(
            "config: {source}[{first}] and {source}[{second}] would both back up as \
             `{source}-default`; give one of them a distinct `name`"
        );
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

/// Error if `value` isn't a bare host written as the `url` crate spells it: it's
/// interpolated as `https://<domain>…` and set as `cleanup reddit`'s host, and `reddit_key`
/// compares it to a parsed URL's host, so a scheme, slash, whitespace, port, or any form
/// the parser rewrites (percent-escapes, non-ASCII) would corrupt the URLs or never match.
/// A trailing `.` would be set as the host, which `cleanup reddit`'s reddit.com filter
/// then no longer recognizes.
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
/// generically (see [`resolve_setting`] and `timefmt::resolve_dates`). The method names
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

/// Resolve a setting through its precedence tiers, highest first: the CLI flag, else
/// the account override, else the per-source default, else the resolved `[pinboard]`
/// global. The first present (`Some`) value wins; the global is the guaranteed fallback.
pub fn resolve_setting<T>(flag: Option<T>, account: Option<T>, source: Option<T>, global: T) -> T {
    flag.or(account).or(source).unwrap_or(global)
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
mod tests;
