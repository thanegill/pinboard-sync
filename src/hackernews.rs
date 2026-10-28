//! The HackerNews source: a user's public favorites. Scrapes the favorites pages
//! (`/favorites?id=<user>` and `&comments=t`) for item IDs, then batch-reads their
//! details from the Algolia HN search API. Favorited *stories* bookmark the linked
//! article (with the HN discussion in the notes); favorited *comments* bookmark the
//! HN permalink.

mod algolia;
pub mod cleanup;
mod favorites;

pub use algolia::AlgoliaClient;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use log::warn;
use serde::Deserialize;

use crate::backup::{BackupDump, BackupSource, RawSink};
use crate::bookmark::Bookmark;
use crate::htmltext::{blockquote, html_to_markdown, html_to_plain};
use crate::http::{client_builder, RetryingClient};
use crate::keys::{HackerNewsKeys, RedditKeys};
use crate::source::{BookmarkDraft, Source, SourceError, UrlKey};
use crate::tags::{push_prefixed, push_tag, push_tags};
use favorites::{FavoritesStream, FAVORITES_PAGE_PAUSE};
use url::Url;

/// HN blocks some default User-Agents on the HTML pages, so present a browser one.
const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";
const HN_BASE: &str = "https://news.ycombinator.com";
const MAX_RETRIES: u32 = 4;
const RETRY_DELAY: Duration = Duration::from_secs(2);

/// Tag vocabulary for HackerNews favorites. `tags` are applied to every bookmark
/// (defaulting to `["hackernews"]`); the rest default to their built-in values, and
/// an empty string disables that tag.
#[derive(Debug, Clone)]
pub struct HackerNewsConfig {
    pub tags: Vec<String>,
    pub comment: String,
    pub author_prefix: String,
    /// Prefix for the tag derived from a `Show/Ask/Tell/Launch HN:` title.
    pub special_prefix: String,
    /// Marker tag for `cleanup --link-discussions`: bookmarks carrying it are
    /// looked up on HN by URL and linked to their discussion.
    pub link_tag: String,
}

impl Default for HackerNewsConfig {
    fn default() -> Self {
        Self {
            tags: vec!["hackernews".into()],
            comment: "hackernews-comment".into(),
            author_prefix: "author:hackernews:".into(),
            special_prefix: "hackernews:".into(),
            link_tag: "find-hn".into(),
        }
    }
}

/// A normalized HackerNews item (built from an Algolia hit).
#[derive(Debug, Clone, Deserialize)]
struct HackerNewsItem {
    id: u64,
    #[serde(rename = "type", default)]
    kind: HackerNewsItemKind,
    #[serde(default)]
    by: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    text: Option<String>,
    /// Item creation time (unix epoch seconds), when known.
    #[serde(default)]
    created_at: Option<i64>,
}

/// What an HN item is. Algolia carries it in a hit's `_tags`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum HackerNewsItemKind {
    #[default]
    Story,
    Comment,
    Poll,
    Job,
}

impl HackerNewsItemKind {
    /// HN's own name for the kind, as `_tags` spells it.
    fn as_str(self) -> &'static str {
        match self {
            HackerNewsItemKind::Story => "story",
            HackerNewsItemKind::Comment => "comment",
            HackerNewsItemKind::Poll => "poll",
            HackerNewsItemKind::Job => "job",
        }
    }
}

impl std::fmt::Display for HackerNewsItemKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl HackerNewsItem {
    /// Shape the item into a Pinboard draft. Comments keep the HN permalink; stories
    /// bookmark the linked article (HN discussion in the notes), falling back to the HN
    /// permalink for text posts (Ask HN, etc.). `keys` keys the article.
    fn into_draft(self, cfg: &HackerNewsConfig, keys: &HackerNewsKeys) -> Option<BookmarkDraft> {
        let item_id = HackerNewsItemId::from(self.id);
        // The HN discussion permalink (always valid — the id sits in the query string).
        let hn_url = Url::from(&item_id);
        let is_comment = self.kind == HackerNewsItemKind::Comment;

        let mut tags = Vec::new();
        push_tags(&mut tags, &cfg.tags);
        if is_comment {
            push_tag(&mut tags, &cfg.comment);
        }
        push_prefixed(&mut tags, &cfg.author_prefix, &self.by);
        if let Some(special) = self.title.as_deref().and_then(special_type) {
            push_prefixed(&mut tags, &cfg.special_prefix, &special);
        }

        if is_comment {
            let md = html_to_markdown(&self.text.unwrap_or_default());
            return Some(BookmarkDraft {
                bookmark: Bookmark {
                    url: hn_url,
                    title: html_to_plain(&format!("HN: Comment by {}", self.by)),
                    note: if md.is_empty() { md } else { blockquote(&md) },
                    tags,
                    timestamp: self.created_at.and_then(crate::timefmt::from_unix),
                    public: false,
                    read_later: false,
                },
                dedup_key: item_id.dedup_key(),
            });
        }

        let article = self.url.filter(|s| !s.is_empty());
        let (url, dedup_key) = match article.as_deref() {
            Some(u) => match Url::parse(u) {
                Ok(parsed) => {
                    let key = keys
                        .dedup_key(&parsed)
                        .unwrap_or_else(|| parsed.to_string());
                    (parsed, key)
                }
                Err(e) => {
                    warn!("skipping HN story with unparseable article URL {u}: {e}");
                    return None;
                }
            },
            None => (hn_url.clone(), item_id.dedup_key()),
        };
        let title = html_to_plain(
            &self
                .title
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("HN: {} by {}", self.kind, self.by)),
        );
        // The `HN Link:` line points at the discussion. For a text post the bookmark
        // URL already *is* that permalink, so including it would duplicate the URL —
        // skip it and let the notes carry just the post text.
        let mut note = if article.is_some() {
            format!("HN Link: {hn_url}")
        } else {
            String::new()
        };
        if let Some(text) = self.text.filter(|s| !s.is_empty()) {
            // Convert the raw Algolia HTML to Markdown, wrapped in a <blockquote>.
            let block = blockquote(&html_to_markdown(&text));
            note = if note.is_empty() {
                block
            } else {
                format!("{note}\n\n{block}")
            };
        }
        Some(BookmarkDraft {
            bookmark: Bookmark {
                url,
                title,
                note,
                tags,
                timestamp: self.created_at.and_then(crate::timefmt::from_unix),
                public: false,
                read_later: false,
            },
            dedup_key,
        })
    }
}

/// A client for HackerNews requests, the favorites pages and Algolia alike.
fn retrying_client() -> anyhow::Result<RetryingClient> {
    let http = client_builder()
        .user_agent(BROWSER_UA)
        .build()
        .context("building HTTP client")?;
    Ok(RetryingClient::new(http, MAX_RETRIES, RETRY_DELAY))
}

/// Reads a user's public HackerNews favorites.
pub struct HackerNewsClient {
    http: RetryingClient,
    username: String,
    config: HackerNewsConfig,
    /// Keys favorites, with every configured Reddit account's domain as a Reddit host.
    keys: HackerNewsKeys,
    /// HN site base (overridden in tests).
    base: String,
    /// Looks the favorited items up.
    algolia: AlgoliaClient,
}

impl HackerNewsClient {
    pub fn new(username: String, config: HackerNewsConfig) -> anyhow::Result<Self> {
        Self::build(username, config, HN_BASE.to_string(), AlgoliaClient::new()?)
    }

    fn build(
        username: String,
        config: HackerNewsConfig,
        base: String,
        algolia: AlgoliaClient,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            http: retrying_client()?,
            username,
            config,
            keys: HackerNewsKeys::default(),
            base,
            algolia,
        })
    }

    /// The client keying an article on any of `domains` as Reddit's own dedup key does,
    /// so it still matches once `cleanup reddit` has moved it onto that domain.
    #[must_use]
    pub fn with_reddit_domains(self, domains: Vec<String>) -> Self {
        Self {
            keys: HackerNewsKeys::new(RedditKeys::new(domains)),
            ..self
        }
    }
}

impl Source for HackerNewsClient {
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError> {
        self.fetch_capturing(None, &mut RawSink::disabled()).await
    }

    /// Favorites list newest first, so each stream stops paging at its first page that
    /// would give `sync` nothing to write.
    async fn fetch_incremental(
        &self,
        existing: &[Bookmark],
    ) -> Result<Vec<BookmarkDraft>, SourceError> {
        self.fetch_capturing(Some(&self.synced_keys(existing)), &mut RawSink::disabled())
            .await
    }
}

impl HackerNewsClient {
    /// [`Source::fetch`], pushing the scraped favorites pages and every Algolia response
    /// to `sink` so `backup` gets both verbatim out of the same traversal. With
    /// `synced_keys`, each stream stops at its first page with nothing new, and items are
    /// looked up page by page to tell; without, every page is read and the items are
    /// looked up in batches at the end.
    async fn fetch_capturing(
        &self,
        synced_keys: Option<&HashSet<String>>,
        sink: &mut RawSink,
    ) -> Result<Vec<BookmarkDraft>, SourceError> {
        let stories = self
            .collect_ids(FavoritesStream::Stories, synced_keys, sink)
            .await?;
        crate::http::sleep(FAVORITES_PAGE_PAUSE).await;
        let comments = self
            .collect_ids(FavoritesStream::Comments, synced_keys, sink)
            .await?;
        let mut ids = stories.ids;
        ids.extend(comments.ids);
        let mut drafts = stories.drafts;
        drafts.extend(comments.drafts);
        // De-dup IDs while preserving order (newest first).
        let mut seen = HashSet::new();
        ids.retain(|id| seen.insert(id.clone()));

        if synced_keys.is_none() {
            drafts = self.drafts_by_id(self.algolia.fetch_items(&ids, sink).await?);
        }
        Ok(ids.iter().filter_map(|id| drafts.remove(id)).collect())
    }

    /// Each item shaped into its draft, keyed by id; items that make none are dropped.
    fn drafts_by_id(
        &self,
        items: HashMap<HackerNewsItemId, HackerNewsItem>,
    ) -> HashMap<HackerNewsItemId, BookmarkDraft> {
        items
            .into_iter()
            .filter_map(|(id, item)| {
                item.into_draft(&self.config, &self.keys)
                    .map(|draft| (id, draft))
            })
            .collect()
    }
}

impl BackupSource for HackerNewsClient {
    async fn dump(&self) -> Result<BackupDump, SourceError> {
        let mut sink = RawSink::collecting();
        let drafts = self.fetch_capturing(None, &mut sink).await?;
        Ok(BackupDump::from_drafts(sink, &drafts))
    }
}

impl UrlKey for HackerNewsClient {
    fn dedup_key(&self, url: &Url) -> Option<String> {
        self.keys.dedup_key(url)
    }
}

/// A HackerNews item id — the `<n>` in a `news.ycombinator.com/item?id=<n>` URL. Held
/// as a string (the favorites page scrapes it as text and Algolia's `objectID` filter
/// takes it as text); built from a numeric id, a scraped string, or parsed out of a URL
/// (`TryFrom<&Url>`), and rendered back to the discussion permalink (`From<&Self> for Url`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HackerNewsItemId(String);

impl std::fmt::Display for HackerNewsItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl HackerNewsItemId {
    /// The key `sync` dedups a favorited HN item on: `hn:<id>`, matching however the
    /// bookmark is stored.
    pub(crate) fn dedup_key(&self) -> String {
        format!("hn:{self}")
    }
}

impl From<u64> for HackerNewsItemId {
    fn from(id: u64) -> Self {
        Self(id.to_string())
    }
}

impl From<String> for HackerNewsItemId {
    fn from(id: String) -> Self {
        Self(id)
    }
}

/// Parse the item id out of an HN `item?id=<n>` URL (host must be exactly
/// `news.ycombinator.com`). `Err` for any non-HN-item URL.
impl TryFrom<&Url> for HackerNewsItemId {
    type Error = NotHnItemUrl;
    fn try_from(url: &Url) -> Result<Self, Self::Error> {
        if url.host_str() != Some("news.ycombinator.com")
            || !url.path().trim_start_matches('/').starts_with("item")
        {
            return Err(NotHnItemUrl);
        }
        let id: String = url
            .query_pairs()
            .find(|(key, _)| key == "id")
            .map(|(_, id)| id.into_owned())
            .ok_or(NotHnItemUrl)?
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        if id.is_empty() {
            return Err(NotHnItemUrl);
        }
        Ok(Self(id))
    }
}

/// The HN discussion permalink for an item id (`news.ycombinator.com/item?id=<n>`). The
/// id only ever sits in the query string, so this always parses.
impl From<&HackerNewsItemId> for Url {
    fn from(id: &HackerNewsItemId) -> Self {
        Url::parse(&format!("{HN_BASE}/item?id={}", id.0))
            .expect("HN item permalink is a valid URL")
    }
}

/// The URL isn't an HN `item?id=<n>` permalink (so it has no [`HackerNewsItemId`]).
#[derive(Debug, thiserror::Error)]
#[error("not a HackerNews item URL")]
pub struct NotHnItemUrl;

/// The slug for a `Show/Ask/Tell/Launch HN:` title prefix (e.g. `Show HN:` →
/// `show-hn`), or `None` for ordinary titles.
fn special_type(title: &str) -> Option<String> {
    ["Show HN", "Ask HN", "Tell HN", "Launch HN"]
        .into_iter()
        .find(|kind| {
            title
                .strip_prefix(kind)
                .is_some_and(|rest| rest.starts_with(':'))
        })
        .map(|kind| kind.to_lowercase().replace(' ', "-"))
}

#[cfg(test)]
impl HackerNewsClient {
    pub(crate) fn with_base_urls(
        username: String,
        config: HackerNewsConfig,
        base: String,
        algolia: String,
    ) -> Self {
        Self::build(
            username,
            config,
            base,
            AlgoliaClient::build(algolia).unwrap(),
        )
        .unwrap()
    }

    /// The draft `sync` makes for a favorited story with this article `url`.
    fn story_draft(&self, url: &str) -> BookmarkDraft {
        let story = serde_json::json!({ "id": 42, "type": "story", "by": "a", "title": "T",
                                        "url": url });
        serde_json::from_value::<HackerNewsItem>(story)
            .unwrap()
            .into_draft(&self.config, &self.keys)
            .unwrap()
    }

    /// The account after one `sync` of `drafts` onto `account`.
    async fn sync_once(&self, drafts: &[BookmarkDraft], account: &[Bookmark]) -> Vec<Bookmark> {
        let settings = crate::sync::JobSettings {
            limit: crate::config::SyncLimit::All,
            read_later: false,
            public: false,
            dates: crate::test_support::NO_DATING,
        };
        let new = crate::sync::prepare_new_drafts(
            self,
            drafts.to_vec(),
            account,
            &settings,
            time::OffsetDateTime::UNIX_EPOCH,
        );
        let pinboard = crate::test_support::FakePinboard::default();
        let tally = crate::sync::write_drafts(&pinboard, &new, false).await;
        assert_eq!(tally.failed, 0);
        let ops = pinboard.ops.take();
        crate::test_support::replay(account, &ops)
    }
}

#[cfg(test)]
mod tests;

/// Integration tests against a `wiremock` server. These bind a TCP socket, so they
/// can't run in the Nix build sandbox — the flake skips `net_tests` there.
#[cfg(test)]
mod net_tests;
