//! The HackerNews source: a user's public favorites, scraped for item IDs and read from
//! Algolia. A story bookmarks its article, with the discussion in the notes; a comment
//! bookmarks its permalink.

mod algolia;
pub mod cleanup;
mod favorites;

pub use algolia::AlgoliaClient;

use std::collections::{HashMap, HashSet};

use anyhow::{Context, Result};
use log::warn;
use serde::Deserialize;

use crate::backup::{BackupDump, BackupSource, RawSink};
use crate::bookmark::Bookmark;
use crate::htmltext::{blockquote, html_to_markdown, html_to_plain};
use crate::http::{client_builder, RetryingClient, DEFAULT_BACKOFF};
use crate::keys::{HackerNewsKeys, RedditKeys};
use crate::source::{BookmarkDraft, DedupKey, Source, SourceError, UrlKey};
use crate::tags::Tags;
use favorites::{FavoritesStream, FAVORITES_PAGE_PAUSE};
use url::Url;

/// HN blocks some default User-Agents on the HTML pages, so present a browser one.
const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";
const HN_BASE: &str = "https://news.ycombinator.com";

/// Starts the note line linking to the HN discussion, which `cleanup` reads back.
const HN_LINK: &str = "HN Link:";

/// The note line linking to the HN discussion at `url`.
fn hn_link_line(url: impl std::fmt::Display) -> String {
    format!("{HN_LINK} {url}")
}

/// The tags for HackerNews favorites; an empty one is disabled.
#[derive(Debug, Clone)]
pub struct HackerNewsConfig {
    pub tags: Vec<String>,
    pub comment: String,
    pub author_prefix: String,
    /// Prefix for the tag derived from a `Show/Ask/Tell/Launch HN:` title.
    pub special_prefix: String,
    /// Marks the bookmarks `cleanup --link-discussions` looks up on HN by URL, to link each
    /// one found to its HN discussion.
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
    /// Epoch seconds.
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
    /// The item's draft: a comment keeps its permalink, a story bookmarks its article, or
    /// for a text post its permalink. `keys` keys the article.
    fn into_draft(self, cfg: &HackerNewsConfig, keys: &HackerNewsKeys) -> Option<BookmarkDraft> {
        let item_id = HackerNewsItemId::from(self.id);
        let hn_url = Url::from(&item_id);
        let is_comment = self.kind == HackerNewsItemKind::Comment;

        let mut tags = Tags::default();
        tags.push_all(&cfg.tags);
        if is_comment {
            tags.push(&cfg.comment);
        }
        tags.push_prefixed(&cfg.author_prefix, &self.by);
        if let Some(special) = self.title.as_deref().and_then(special_type) {
            tags.push_prefixed(&cfg.special_prefix, &special);
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
                        .unwrap_or_else(|| parsed.as_str().into());
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
        // A text post's bookmark URL already is the discussion.
        let mut note = if article.is_some() {
            hn_link_line(&hn_url)
        } else {
            String::new()
        };
        if let Some(text) = self.text.filter(|s| !s.is_empty()) {
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
    Ok(RetryingClient::new(http, DEFAULT_BACKOFF))
}

/// Reads a user's public HackerNews favorites.
pub struct HackerNewsClient {
    http: RetryingClient,
    username: String,
    config: HackerNewsConfig,
    /// Keys favorites, with every configured Reddit account's domain as a Reddit host.
    keys: HackerNewsKeys,
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

    /// The client, keying an article on any of `domains` as Reddit's key does, so it still
    /// matches once `cleanup reddit` moves it there.
    #[must_use]
    pub fn with_reddit_domains(self, domains: Vec<String>) -> Self {
        Self {
            keys: HackerNewsKeys::new(RedditKeys::new(domains)),
            ..self
        }
    }

    /// [`Source::fetch`], pushing the scraped pages and every Algolia response to `sink`.
    /// With `synced_keys`, each stream stops at its first page with nothing new, looking
    /// items up page by page to tell; without, items are looked up in batches at the end.
    async fn fetch_capturing(
        &self,
        synced_keys: Option<&HashSet<DedupKey>>,
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
        let mut seen = HashSet::new();
        ids.retain(|id| seen.insert(id.clone()));

        if synced_keys.is_none() {
            drafts = self.drafts_by_id(self.algolia.fetch_items(&ids, sink).await?);
        }
        Ok(ids.iter().filter_map(|id| drafts.remove(id)).collect())
    }

    /// Each item's draft, keyed by id.
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

impl BackupSource for HackerNewsClient {
    async fn dump(&self) -> Result<BackupDump, SourceError> {
        let mut sink = RawSink::collecting();
        let drafts = self.fetch_capturing(None, &mut sink).await?;
        Ok(BackupDump::from_drafts(sink, &drafts))
    }
}

impl UrlKey for HackerNewsClient {
    fn dedup_key(&self, url: &Url) -> Option<DedupKey> {
        self.keys.dedup_key(url)
    }
}

/// A HackerNews item id, the `<n>` of `news.ycombinator.com/item?id=<n>`, held as the text
/// both the scrape and Algolia's `objectID` filter use.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct HackerNewsItemId(String);

impl std::fmt::Display for HackerNewsItemId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl HackerNewsItemId {
    /// `hn:<id>`, the key a favorited item dedups on however its bookmark is stored.
    pub(crate) fn dedup_key(&self) -> DedupKey {
        format!("hn:{self}").into()
    }
}

impl From<u64> for HackerNewsItemId {
    fn from(id: u64) -> Self {
        Self(id.to_string())
    }
}

/// A scraped or searched id, all ASCII digits: it goes into Algolia's `objectID:` filter
/// and the `HN Link:` note.
impl std::str::FromStr for HackerNewsItemId {
    type Err = NotHnItemId;
    #[expect(
        clippy::map_err_ignore,
        reason = "past the digit check, an empty or overflowing id is not an item id either"
    )]
    fn from_str(id: &str) -> Result<Self, Self::Err> {
        if !id.bytes().all(|b| b.is_ascii_digit()) {
            return Err(NotHnItemId);
        }
        id.parse::<u64>().map(Self::from).map_err(|_| NotHnItemId)
    }
}

/// The id of a `news.ycombinator.com/item?id=<n>` URL, that host exactly.
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

/// The item's discussion permalink, which always parses: the id sits in the query.
impl From<&HackerNewsItemId> for Url {
    #[expect(
        clippy::expect_used,
        reason = "an id is only digits, which always parse"
    )]
    fn from(id: &HackerNewsItemId) -> Self {
        Url::parse(&format!("{HN_BASE}/item?id={}", id.0))
            .expect("HN item permalink is a valid URL")
    }
}

/// The URL isn't an HN `item?id=<n>` permalink (so it has no [`HackerNewsItemId`]).
#[derive(Debug, thiserror::Error)]
#[error("not a HackerNews item URL")]
pub struct NotHnItemUrl;

/// The text isn't an HN item id, which is all digits.
#[derive(Debug, thiserror::Error)]
#[error("not a HackerNews item id")]
pub struct NotHnItemId;

/// The slug of a `Show/Ask/Tell/Launch HN:` title prefix (`Show HN:` -> `show-hn`).
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
        let tally =
            crate::sync::write_drafts(&pinboard, &new, crate::bookmark::WriteMode::Live).await;
        assert_eq!(tally.failed, 0);
        let ops = pinboard.ops.take();
        crate::test_support::replay(account, &ops)
    }
}

#[cfg(test)]
mod tests;

/// Integration tests against a `wiremock` server. These bind a TCP socket, so they
/// can't run in the Nix build sandbox -- the flake skips `net_tests` there.
#[cfg(test)]
mod net_tests;
