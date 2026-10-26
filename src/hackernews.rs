//! The HackerNews source: a user's public favorites. Scrapes the favorites pages
//! (`/favorites?id=<user>` and `&comments=t`) for item IDs, then batch-reads their
//! details from the Algolia HN search API. Favorited *stories* bookmark the linked
//! article (with the HN discussion in the notes); favorited *comments* bookmark the
//! HN permalink.

pub mod cleanup;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use log::{debug, warn};
use scraper::{Html, Selector};
use serde::Deserialize;

use crate::backup::{BackupDump, BackupSource, RawKind, RawSink};
use crate::bookmark::Bookmark;
use crate::htmltext::{blockquote, html_to_markdown, html_to_plain};
use crate::http::{client_builder, retry_after_up_to, RetryingClient};
use crate::source::{
    page_is_caught_up, push_prefixed, push_tag, push_tags, url_key, BookmarkDraft, Source,
    SourceError, UrlKey,
};
use url::Url;

/// HN blocks some default User-Agents on the HTML pages, so present a browser one.
const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";
const HN_BASE: &str = "https://news.ycombinator.com";
const ALGOLIA_BASE: &str = "https://hn.algolia.com";
/// Item IDs per Algolia `objectID:… OR …` query. The API rejects very long filter
/// strings (~200+ clauses), so stay well under that.
const ITEM_BATCH: usize = 100;
const MAX_RETRIES: u32 = 4;
const RETRY_DELAY: Duration = Duration::from_secs(2);
/// The longest `Retry-After` a favorites-page 429 is waited out for, once. A 429 asking
/// for longer, or for nothing, fails the fetch at once rather than holding the run open.
const FAVORITES_MAX_RETRY_AFTER: Duration = Duration::from_secs(120);
/// The pause before each favorites page after a fetch's first, the comments stream's
/// first page included.
const FAVORITES_PAGE_PAUSE: Duration = Duration::from_millis(500);
/// The pause before each Algolia item chunk after the first.
const ALGOLIA_CHUNK_PAUSE: Duration = Duration::from_millis(300);
/// Cap on favorites pages followed via the "More" link (30 items per page, so this
/// covers thousands of favorites). Bounds a malformed or looping "More" link so the
/// scrape can't spin forever.
const MAX_FAVORITES_PAGES: usize = 100;

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

/// An Algolia HN search response.
#[derive(Debug, Deserialize)]
struct AlgoliaSearchResponse {
    #[serde(default)]
    hits: Vec<AlgoliaHit>,
}

/// One hit from the Algolia HN search API.
#[derive(Debug, Deserialize)]
struct AlgoliaHit {
    #[serde(rename = "objectID")]
    object_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    story_text: Option<String>,
    #[serde(default)]
    comment_text: Option<String>,
    #[serde(rename = "_tags", default)]
    tags: Vec<String>,
    /// Item creation time (unix epoch seconds).
    #[serde(default)]
    created_at_i: Option<i64>,
}

impl From<AlgoliaHit> for HackerNewsItem {
    fn from(h: AlgoliaHit) -> Self {
        // The item type is carried in `_tags` (e.g. "story"/"comment"/"poll"/"job").
        let kind = [
            HackerNewsItemKind::Comment,
            HackerNewsItemKind::Poll,
            HackerNewsItemKind::Job,
            HackerNewsItemKind::Story,
        ]
        .into_iter()
        .find(|kind| h.tags.iter().any(|tag| tag == kind.as_str()))
        .unwrap_or_default();
        HackerNewsItem {
            id: h.object_id.parse().unwrap_or(0),
            kind,
            by: h.author.unwrap_or_default(),
            url: h.url,
            title: h.title,
            text: h.comment_text.or(h.story_text),
            created_at: h.created_at_i,
        }
    }
}

impl HackerNewsItem {
    /// Shape the item into a Pinboard draft. Comments keep the HN permalink; stories
    /// bookmark the linked article (HN discussion in the notes), falling back to the
    /// HN permalink for text posts (Ask HN, etc.).
    fn into_draft(self, cfg: &HackerNewsConfig) -> Option<BookmarkDraft> {
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
                    // An article URL that is itself an HN item permalink (e.g. a meta
                    // post) must key on `hn:<id>` to match how the stored bookmark reads
                    // back through `HackerNewsClient::dedup_key`; a generic host+path key
                    // would drop the id query and re-add the bookmark every sync.
                    let key = HackerNewsItemId::try_from(&parsed)
                        .map(|id| id.dedup_key())
                        .unwrap_or_else(|_| url_key(&parsed).unwrap_or_else(|| parsed.to_string()));
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

/// Reads a user's public HackerNews favorites.
pub struct HackerNewsClient {
    http: RetryingClient,
    username: String,
    config: HackerNewsConfig,
    /// HN site base (overridden in tests).
    base: String,
    /// Algolia API base (overridden in tests).
    algolia: String,
}

impl HackerNewsClient {
    pub fn new(username: String, config: HackerNewsConfig) -> anyhow::Result<Self> {
        Self::build(
            username,
            config,
            HN_BASE.to_string(),
            ALGOLIA_BASE.to_string(),
        )
    }

    fn build(
        username: String,
        config: HackerNewsConfig,
        base: String,
        algolia: String,
    ) -> anyhow::Result<Self> {
        let http = client_builder()
            .user_agent(BROWSER_UA)
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http: RetryingClient::new(http, MAX_RETRIES, RETRY_DELAY),
            username,
            config,
            base,
            algolia,
        })
    }

    /// Collect favorited item IDs from the stories page (`comments=false`) or the
    /// comments page (`comments=true`), following the "More" link across pages. With
    /// `synced_keys`, each page's items are looked up as it is read and shaped into
    /// `drafts`, and the walk stops at the first page that has nothing new
    /// ([`is_caught_up`]).
    async fn collect_ids(
        &self,
        comments: bool,
        synced_keys: Option<&HashSet<String>>,
        out: &mut Vec<HackerNewsItemId>,
        drafts: &mut HashMap<HackerNewsItemId, BookmarkDraft>,
        sink: &mut RawSink,
    ) -> Result<(), SourceError> {
        // The scraped page is the only record of a favorite HN's API never exposes, so
        // `backup` keeps it verbatim alongside the Algolia details.
        let stream = if comments {
            "favorites-comments"
        } else {
            "favorites"
        };
        let label = format!("hn {stream}");
        let suffix = if comments { "&comments=t" } else { "" };
        let mut next = Some(format!(
            "{}/favorites?id={}{}",
            self.base, self.username, suffix
        ));
        let mut visited = HashSet::new();
        let mut page = 0usize;
        while let Some(url) = next.take() {
            page += 1;
            if page > MAX_FAVORITES_PAGES {
                warn!(
                    "hn {stream} for '{}': hit the {MAX_FAVORITES_PAGES}-page cap; \
                     stopping (some favorites may be missing)",
                    self.username
                );
                sink.mark_truncated(format!(
                    "hn {stream} stopped at the {MAX_FAVORITES_PAGES}-page cap"
                ));
                break;
            }
            if !visited.insert(url.clone()) {
                warn!(
                    "hn {stream} for '{}': 'More' link looped back to {url}; stopping",
                    self.username
                );
                break;
            }
            if page > 1 {
                crate::http::sleep(FAVORITES_PAGE_PAUSE).await;
            }
            let resp = self
                .http
                .send_with(&label, retry_after_up_to(FAVORITES_MAX_RETRY_AFTER), || {
                    self.http.get(&url)
                })
                .await?;
            let status = resp.status();
            if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
                let retry_after = resp
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|value| value.to_str().ok())
                    .map(|value| format!(" (Retry-After: {value})"))
                    .unwrap_or_default();
                return Err(SourceError::RateLimited(format!(
                    "hn {stream} for '{}' returned {status}{retry_after}",
                    self.username
                )));
            }
            let body = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                return Err(anyhow::anyhow!(
                    "hn {stream} for '{}' returned {status}",
                    self.username
                )
                .into());
            }
            sink.push(stream, RawKind::Html, &body);
            let (ids, more) = match parse_favorite_ids(&body) {
                FavoritesPage::Recognized { ids, more } => (ids, more),
                FavoritesPage::Unrecognized => {
                    return Err(anyhow::anyhow!(
                        "hn {stream} for '{}' had no recognizable favorites structure \
                         (page {page}); HN's markup may have changed",
                        self.username
                    )
                    .into());
                }
            };
            let ids: Vec<HackerNewsItemId> = ids.into_iter().map(HackerNewsItemId::from).collect();
            let caught_up = match synced_keys {
                Some(synced_keys) => {
                    let page_drafts = self.drafts_by_id(self.fetch_items(&ids, sink).await?);
                    let caught_up = is_caught_up(&ids, &page_drafts, synced_keys);
                    drafts.extend(page_drafts);
                    caught_up
                }
                None => false,
            };
            out.extend(ids);
            if caught_up {
                debug!(
                    "hn {stream} for '{}': page {page} has nothing new; stopping",
                    self.username
                );
                break;
            }
            next = more.and_then(|href| self.resolve(&href, stream));
        }
        Ok(())
    }

    /// Resolve an HN "More" href (typically relative, e.g. `favorites?id=u&p=2`)
    /// against the favorites origin. Relative hrefs resolve against `base`; an
    /// absolute href whose host differs from `base` is refused (returns `None`)
    /// so a hostile favorites page can't steer the crawler at an arbitrary host.
    /// `stream` names the favorites page the link came from, for the warning.
    fn resolve(&self, href: &str, stream: &str) -> Option<String> {
        let base = Url::parse(&self.base).ok()?;
        let resolved = base.join(href).ok()?;
        if resolved.host_str() != base.host_str() {
            warn!(
                "hn {stream} for '{}': ignoring 'More' link to foreign host {resolved}",
                self.username
            );
            return None;
        }
        Some(resolved.to_string())
    }

    /// Batch-fetch item details by ID from the Algolia HN search API, keyed by ID.
    /// IDs are queried in chunks of [`ITEM_BATCH`] via `objectID:… OR …`; items that
    /// no longer exist are simply absent from the map.
    async fn fetch_items(
        &self,
        ids: &[HackerNewsItemId],
        sink: &mut RawSink,
    ) -> Result<HashMap<HackerNewsItemId, HackerNewsItem>, SourceError> {
        let endpoint = format!("{}/api/v1/search", self.algolia);
        let mut out = HashMap::new();
        for (i, chunk) in ids.chunks(ITEM_BATCH).enumerate() {
            if i > 0 {
                crate::http::sleep(ALGOLIA_CHUNK_PAUSE).await;
            }
            let filters = chunk
                .iter()
                .map(|id| format!("objectID:{id}"))
                .collect::<Vec<_>>()
                .join(" OR ");
            let hits_per_page = chunk.len().to_string();
            let resp = self
                .http
                .send("hn algolia", || {
                    self.http.get(&endpoint).query(&[
                        ("filters", filters.as_str()),
                        ("hitsPerPage", hits_per_page.as_str()),
                    ])
                })
                .await?;
            let resp = algolia_success(resp, "hn algolia").await?;
            let body = resp.text().await.context("reading hn algolia response")?;
            sink.push("", RawKind::Json, &body);
            let search: AlgoliaSearchResponse =
                serde_json::from_str(&body).context("parsing hn algolia response")?;
            for hit in search.hits {
                let item = HackerNewsItem::from(hit);
                out.insert(HackerNewsItemId::from(item.id), item);
            }
        }
        Ok(out)
    }

    /// Find the HN story id discussing `url`, via the Algolia URL search. Returns
    /// the id only when a hit's URL matches `url` (so loose matches are ignored).
    async fn search_by_url(&self, url: &Url) -> Result<Option<HackerNewsItemId>, SourceError> {
        let endpoint = format!("{}/api/v1/search", self.algolia);
        let resp = self
            .http
            .send("hn algolia url", || {
                self.http.get(&endpoint).query(&[
                    ("restrictSearchableAttributes", "url"),
                    ("query", url.as_str()),
                    ("hitsPerPage", "5"),
                ])
            })
            .await?;
        let resp = algolia_success(resp, "hn algolia url search").await?;
        let search: AlgoliaSearchResponse =
            resp.json().await.context("parsing hn algolia response")?;
        let want = url_key(url);
        Ok(search.hits.into_iter().find_map(|h| {
            let hit_key = h
                .url
                .as_deref()
                .and_then(|s| Url::parse(s).ok())
                .and_then(|u| url_key(&u));
            (want.is_some() && hit_key == want).then(|| HackerNewsItemId::from(h.object_id))
        }))
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
        let mut ids: Vec<HackerNewsItemId> = Vec::new();
        let mut drafts = HashMap::new();
        self.collect_ids(false, synced_keys, &mut ids, &mut drafts, sink)
            .await?;
        crate::http::sleep(FAVORITES_PAGE_PAUSE).await;
        self.collect_ids(true, synced_keys, &mut ids, &mut drafts, sink)
            .await?;
        // De-dup IDs while preserving order (newest first).
        let mut seen = HashSet::new();
        ids.retain(|id| seen.insert(id.clone()));

        if synced_keys.is_none() {
            drafts = self.drafts_by_id(self.fetch_items(&ids, sink).await?);
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
            .filter_map(|(id, item)| item.into_draft(&self.config).map(|draft| (id, draft)))
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
    /// A favorited HN *item* keys on its id (`hn:<id>`); an article bookmark falls back
    /// to the generic host+path key.
    fn dedup_key(&self, url: &Url) -> Option<String> {
        HackerNewsItemId::try_from(url)
            .ok()
            .map(|id| id.dedup_key())
            .or_else(|| url_key(url))
    }
}

/// `resp` when it succeeded, else an error naming the `what` request with its status and
/// an excerpt of the body.
async fn algolia_success(
    resp: reqwest::Response,
    what: &str,
) -> Result<reqwest::Response, SourceError> {
    let status = resp.status();
    if status.is_success() {
        return Ok(resp);
    }
    let body = resp.text().await.unwrap_or_default();
    Err(crate::http::error_with_body(&format!("{what} returned {status}"), &body, &[]).into())
}

/// Whether a favorites page would give `sync` nothing to write: every item on it that
/// has a draft in `drafts` has its `dedup_key` among `synced_keys`. Favorites list newest
/// first, so the pages after it hold nothing new either. A page whose items make no draft
/// at all (gone from Algolia, or unparseable) is caught up too; an empty page proves
/// nothing.
fn is_caught_up(
    page: &[HackerNewsItemId],
    drafts: &HashMap<HackerNewsItemId, BookmarkDraft>,
    synced_keys: &HashSet<String>,
) -> bool {
    page_is_caught_up(
        page.len(),
        page.iter().filter_map(|id| drafts.get(id)),
        synced_keys,
    )
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
    fn dedup_key(&self) -> String {
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
            .take_while(|c| c.is_ascii_digit())
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
#[derive(Debug)]
pub struct NotHnItemUrl;

/// Outcome of parsing a favorites page. `Unrecognized` means the page had neither
/// favorite rows nor the favorites-page marker: HN's markup likely changed, so the
/// scrape should fail loudly rather than silently import nothing.
enum FavoritesPage {
    Recognized {
        ids: Vec<String>,
        more: Option<String>,
    },
    Unrecognized,
}

/// Parse a favorites page: the favorited item IDs (the `id` of each `tr.athing`
/// row) and the "More" link href, if any. A page with no rows is only accepted as a
/// (genuinely empty) favorites list when it carries HN's `<html op="favorites">`
/// marker; otherwise it is [`FavoritesPage::Unrecognized`]. Pure, so it's unit-tested
/// on sample HTML.
fn parse_favorite_ids(html: &str) -> FavoritesPage {
    let doc = Html::parse_document(html);
    let row = Selector::parse("tr.athing").unwrap();
    let ids: Vec<String> = doc
        .select(&row)
        .filter_map(|e| e.value().attr("id").map(str::to_string))
        .collect();
    let marker = Selector::parse(r#"html[op="favorites"]"#).unwrap();
    if ids.is_empty() && doc.select(&marker).next().is_none() {
        return FavoritesPage::Unrecognized;
    }
    let more = Selector::parse("a.morelink").unwrap();
    let next = doc
        .select(&more)
        .next()
        .and_then(|e| e.value().attr("href").map(str::to_string));
    FavoritesPage::Recognized { ids, more: next }
}

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
        Self::build(username, config, base, algolia).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(value: serde_json::Value) -> HackerNewsItem {
        serde_json::from_value(value).unwrap()
    }

    fn url(s: &str) -> Url {
        Url::parse(s).unwrap()
    }

    fn ids(items: &[u64]) -> Vec<HackerNewsItemId> {
        items.iter().copied().map(HackerNewsItemId::from).collect()
    }

    #[test]
    fn a_page_is_caught_up_only_when_it_has_no_new_draft() {
        let items: HashMap<HackerNewsItemId, HackerNewsItem> = [
            json!({ "id": 1, "type": "comment", "by": "a", "text": "c" }),
            json!({ "id": 2, "type": "story", "by": "a", "title": "T",
                    "url": "https://example.com/two" }),
            json!({ "id": 3, "type": "story", "by": "a", "title": "Ask HN: q" }),
            json!({ "id": 4, "type": "comment", "by": "a", "text": "new" }),
            json!({ "id": 6, "type": "story", "by": "a", "url": "not a url" }),
        ]
        .into_iter()
        .map(|value| {
            let item = item(value);
            (HackerNewsItemId::from(item.id), item)
        })
        .collect();
        let client = HackerNewsClient::new("u".into(), HackerNewsConfig::default()).unwrap();
        let synced_keys: HashSet<String> = [
            "https://news.ycombinator.com/item?id=1",
            "https://example.com/two",
            "https://news.ycombinator.com/item?id=3",
        ]
        .into_iter()
        .filter_map(|stored| client.dedup_key(&url(stored)))
        .collect();
        let drafts = client.drafts_by_id(items);
        let caught_up = |page: &[u64]| is_caught_up(&ids(page), &drafts, &synced_keys);

        assert!(
            caught_up(&[1, 3]),
            "a comment and a text post at their permalinks"
        );
        assert!(
            caught_up(&[2]),
            "a story whose article was saved by hand, with no HN Link note"
        );
        assert!(
            caught_up(&[1, 5, 6]),
            "items that make no draft add nothing"
        );
        assert!(caught_up(&[5, 6]), "a page that makes no draft at all");
        assert!(!caught_up(&[4, 1]), "a new favorite");
        assert!(!caught_up(&[1, 4]), "an unsynced older one");
        assert!(!caught_up(&[]), "an empty page");
    }

    #[test]
    fn story_with_url_bookmarks_the_article_with_hn_link_in_notes() {
        let d = item(json!({
            "id": 42, "type": "story", "by": "alice",
            "title": "Cool thing", "url": "https://example.com/x"
        }))
        .into_draft(&HackerNewsConfig::default())
        .unwrap();
        assert_eq!(d.bookmark.url.as_str(), "https://example.com/x");
        assert_eq!(d.bookmark.title, "Cool thing");
        assert_eq!(
            d.bookmark.note,
            "HN Link: https://news.ycombinator.com/item?id=42"
        );
        assert_eq!(d.dedup_key, "example.com/x");
        assert_eq!(
            d.bookmark.tags,
            vec!["hackernews", "author:hackernews:alice"]
        );
    }

    #[test]
    fn story_whose_article_url_is_an_hn_item_keys_on_the_id() {
        // A meta post whose submitted article URL is itself an HN item permalink. The
        // draft is bookmarked at that URL, so its dedup key must match how the stored
        // bookmark reads back through `HackerNewsClient::dedup_key` (`hn:<id>`) — a
        // generic host+path key drops the `id` query and re-adds it every sync.
        let article = "https://news.ycombinator.com/item?id=999";
        let d = item(json!({
            "id": 42, "type": "story", "by": "alice",
            "title": "Meta post", "url": article
        }))
        .into_draft(&HackerNewsConfig::default())
        .unwrap();
        assert_eq!(d.bookmark.url.as_str(), article);
        assert_eq!(d.dedup_key, "hn:999");
        let client = HackerNewsClient::new("u".into(), HackerNewsConfig::default()).unwrap();
        assert_eq!(client.dedup_key(&d.bookmark.url).as_deref(), Some("hn:999"));
    }

    #[test]
    fn text_post_without_url_bookmarks_the_hn_permalink() {
        let d = item(json!({
            "id": 7, "type": "story", "by": "bob",
            "title": "Ask HN: How?", "text": "<p>details</p>"
        }))
        .into_draft(&HackerNewsConfig::default())
        .unwrap();
        assert_eq!(
            d.bookmark.url.as_str(),
            "https://news.ycombinator.com/item?id=7"
        );
        assert_eq!(d.dedup_key, "hn:7");
        // The bookmark URL is already the HN permalink, so the notes carry only the post
        // text (no redundant `HN Link:`), with the inner HTML converted to Markdown.
        assert_eq!(d.bookmark.note, "<blockquote>details</blockquote>");
        // Ask HN: → special-type tag.
        assert!(d.bookmark.tags.contains(&"hackernews:ask-hn".to_string()));
    }

    #[test]
    fn text_post_without_text_has_empty_notes() {
        let d = item(json!({
            "id": 8, "type": "story", "by": "bob", "title": "Ask HN: empty?"
        }))
        .into_draft(&HackerNewsConfig::default())
        .unwrap();
        assert_eq!(
            d.bookmark.url.as_str(),
            "https://news.ycombinator.com/item?id=8"
        );
        // No text and no redundant HN link: nothing to put in the notes.
        assert_eq!(d.bookmark.note, "");
    }

    #[test]
    fn html_title_is_plain_and_html_body_becomes_markdown_blockquote() {
        // An article story (has a url), so the notes keep the `HN Link:` discussion line.
        let d = item(json!({
            "id": 11, "type": "story", "by": "dave",
            "title": "Rust&#x27;s &amp; more", "url": "https://example.com/a",
            "text": "<p>see <a href=\"https://x.com\">x</a> &gt; y</p>"
        }))
        .into_draft(&HackerNewsConfig::default())
        .unwrap();
        // Title: entities decoded, tags stripped, single line.
        assert_eq!(d.bookmark.title, "Rust's & more");
        // Body: HTML converted to Markdown, wrapped in a literal <blockquote>.
        assert_eq!(
            d.bookmark.note,
            "HN Link: https://news.ycombinator.com/item?id=11\n\n<blockquote>see [x](https://x.com) > y</blockquote>"
        );
    }

    #[test]
    fn comment_bookmarks_permalink_with_comment_tag() {
        let d = item(json!({
            "id": 9, "type": "comment", "by": "carol", "text": "my reply"
        }))
        .into_draft(&HackerNewsConfig::default())
        .unwrap();
        assert_eq!(
            d.bookmark.url.as_str(),
            "https://news.ycombinator.com/item?id=9"
        );
        assert_eq!(d.bookmark.title, "HN: Comment by carol");
        assert_eq!(d.bookmark.note, "<blockquote>my reply</blockquote>");
        assert_eq!(d.dedup_key, "hn:9");
        assert_eq!(
            d.bookmark.tags,
            vec![
                "hackernews",
                "hackernews-comment",
                "author:hackernews:carol"
            ]
        );
    }

    #[test]
    fn into_draft_skips_story_with_unparseable_article_url() {
        // A story whose article URL doesn't parse is dropped rather than producing a
        // draft with no URL. (Comments/text posts use the HN permalink, which always
        // parses, so only article stories can hit this.)
        assert!(
            item(json!({ "id": 5, "type": "story", "by": "ann", "url": "not a url" }))
                .into_draft(&HackerNewsConfig::default())
                .is_none()
        );
    }

    #[test]
    fn special_type_slugs_known_prefixes_only() {
        assert_eq!(special_type("Show HN: x").as_deref(), Some("show-hn"));
        assert_eq!(special_type("Ask HN: y").as_deref(), Some("ask-hn"));
        assert_eq!(special_type("Launch HN: z").as_deref(), Some("launch-hn"));
        assert_eq!(special_type("A normal title"), None);
        // Must be the exact `<prefix>:` form.
        assert_eq!(special_type("Show HNde: nope"), None);
    }

    #[test]
    fn parse_favorite_ids_reads_rows_and_more_link() {
        let html = r#"
            <table>
              <tr class="athing" id="111"><td>a</td></tr>
              <tr class="athing comtr" id="222"><td>b</td></tr>
              <tr><td><a class="morelink" href="favorites?id=u&amp;p=2">More</a></td></tr>
            </table>"#;
        let FavoritesPage::Recognized { ids, more } = parse_favorite_ids(html) else {
            panic!("expected a recognized favorites page");
        };
        assert_eq!(ids, vec!["111", "222"]);
        assert_eq!(more.as_deref(), Some("favorites?id=u&p=2"));
    }

    #[test]
    fn resolve_follows_same_host_but_refuses_foreign_host() {
        let client = HackerNewsClient::with_base_urls(
            "u".to_string(),
            HackerNewsConfig::default(),
            "https://news.ycombinator.com".to_string(),
            "https://hn.algolia.com".to_string(),
        );
        // Relative "More" links resolve against the favorites origin.
        assert_eq!(
            client.resolve("favorites?id=u&p=2", "favorites").as_deref(),
            Some("https://news.ycombinator.com/favorites?id=u&p=2")
        );
        // An absolute same-host link is followed.
        assert_eq!(
            client
                .resolve(
                    "https://news.ycombinator.com/favorites?id=u&p=3",
                    "favorites"
                )
                .as_deref(),
            Some("https://news.ycombinator.com/favorites?id=u&p=3")
        );
        // A hostile absolute "More" link to a foreign host is refused (SSRF guard).
        assert_eq!(
            client.resolve("http://169.254.169.254/latest/meta-data/", "favorites"),
            None
        );
    }

    #[test]
    fn parse_favorite_ids_accepts_empty_list_with_marker() {
        // A genuinely empty favorites list: no rows, but the favorites-page marker
        // is present, so it is a recognized (empty) page, not a markup break.
        let html = r#"<html op="favorites"><body><table id="hnmain"></table></body></html>"#;
        let FavoritesPage::Recognized { ids, more } = parse_favorite_ids(html) else {
            panic!("expected an empty favorites list to be recognized");
        };
        assert!(ids.is_empty());
        assert_eq!(more, None);
    }

    #[test]
    fn parse_favorite_ids_flags_markup_break() {
        // A recognizable HTML page whose favorites structure is gone: no rows and no
        // favorites marker means HN changed the markup.
        let html = r#"<html op="news"><body><div>Nothing here.</div></body></html>"#;
        assert!(matches!(
            parse_favorite_ids(html),
            FavoritesPage::Unrecognized
        ));
    }

    #[test]
    fn hn_item_id_extracts_only_from_hn_item_urls() {
        let id =
            HackerNewsItemId::try_from(&url("https://news.ycombinator.com/item?id=42")).unwrap();
        assert_eq!(id.to_string(), "42");
        // And round-trips back to the discussion permalink.
        assert_eq!(
            Url::from(&id).as_str(),
            "https://news.ycombinator.com/item?id=42"
        );
        assert!(HackerNewsItemId::try_from(&url("https://example.com/item?id=42")).is_err());
        assert!(HackerNewsItemId::try_from(&url("https://news.ycombinator.com/news")).is_err());
    }

    #[test]
    fn dedup_key_distinguishes_hn_items_from_articles() {
        let c = HackerNewsClient::new("u".into(), HackerNewsConfig::default()).unwrap();
        assert_eq!(
            c.dedup_key(&url("https://news.ycombinator.com/item?id=42"))
                .as_deref(),
            Some("hn:42")
        );
        assert_eq!(
            c.dedup_key(&url("https://example.com/x")).as_deref(),
            Some("example.com/x")
        );
    }
}

/// Integration tests against a `wiremock` server. These bind a TCP socket, so they
/// can't run in the Nix build sandbox — the flake skips `net_tests` there.
#[cfg(test)]
mod net_tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn fetch_scrapes_favorites_then_reads_items() {
        let hn = MockServer::start().await;
        // Stories favorites page: one story id, no More link. (The `comments`-absent
        // matcher keeps this from also serving the comments-page request below.)
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .and(query_param("id", "psophis"))
            .and(query_param_is_missing("comments"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"<table><tr class="athing" id="42"><td>x</td></tr></table>"#,
                ),
            )
            .mount(&hn)
            .await;
        // Comments favorites page (comments=t): one comment id.
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .and(query_param("comments", "t"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<table><tr class="athing comtr" id="9"><td>c</td></tr></table>"#,
            ))
            .mount(&hn)
            .await;

        // Algolia returns both items (story + comment) in one batched search.
        let algolia = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "hits": [
                    { "objectID": "42", "title": "Cool", "url": "https://example.com/x",
                      "author": "alice", "_tags": ["story", "author_alice", "story_42"] },
                    { "objectID": "9", "comment_text": "hi", "author": "carol",
                      "_tags": ["comment", "author_carol", "story_8"] }
                ]
            })))
            .expect(1) // a single batched query, not one per item
            .mount(&algolia)
            .await;

        let client = HackerNewsClient::with_base_urls(
            "psophis".into(),
            HackerNewsConfig::default(),
            hn.uri(),
            algolia.uri(),
        );
        let drafts = crate::http::recording_waits(client.fetch())
            .await
            .0
            .unwrap();
        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[0].bookmark.url.as_str(), "https://example.com/x");
        assert_eq!(
            drafts[1].bookmark.url.as_str(),
            "https://news.ycombinator.com/item?id=9"
        );
        assert!(drafts[1]
            .bookmark
            .tags
            .contains(&"hackernews-comment".to_string()));
    }

    #[tokio::test]
    async fn fetch_errors_when_favorites_markup_breaks() {
        // A recognizable page whose favorites structure is gone (no rows, no marker)
        // must fail loudly rather than silently importing nothing.
        let hn = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .and(query_param_is_missing("comments"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<html op="favorites"><body><table id="hnmain"></table></body></html>"#,
            ))
            .mount(&hn)
            .await;
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .and(query_param("comments", "t"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(
                    r#"<html op="news"><body><div>Site moved.</div></body></html>"#,
                ),
            )
            .mount(&hn)
            .await;

        let client = HackerNewsClient::with_base_urls(
            "psophis".into(),
            HackerNewsConfig::default(),
            hn.uri(),
            "http://unused.invalid".into(),
        );
        let err = crate::http::recording_waits(client.fetch())
            .await
            .0
            .unwrap_err();
        let SourceError::Other(err) = err else {
            panic!("expected Other, got {err:?}");
        };
        assert!(
            err.to_string()
                .contains("hn favorites-comments for 'psophis'"),
            "the error names the page whose markup broke: {err:#}"
        );
    }

    #[tokio::test]
    async fn fetch_succeeds_on_genuinely_empty_favorites() {
        // Both favorites pages are empty but carry the marker: a recognized empty
        // list, so the fetch succeeds with no drafts (Algolia is never queried).
        let hn = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<html op="favorites"><body><table id="hnmain"></table></body></html>"#,
            ))
            .mount(&hn)
            .await;

        let client = HackerNewsClient::with_base_urls(
            "psophis".into(),
            HackerNewsConfig::default(),
            hn.uri(),
            "http://unused.invalid".into(),
        );
        let drafts = crate::http::recording_waits(client.fetch())
            .await
            .0
            .unwrap();
        assert!(drafts.is_empty());
    }

    fn favorites_client(hn: &MockServer) -> HackerNewsClient {
        HackerNewsClient::with_base_urls(
            "psophis".into(),
            HackerNewsConfig::default(),
            hn.uri(),
            "http://unused.invalid".into(),
        )
    }

    fn favorites_page(ids: &[u64], more: Option<&str>) -> ResponseTemplate {
        let rows: String = ids
            .iter()
            .map(|id| format!(r#"<tr class="athing" id="{id}"><td>x</td></tr>"#))
            .collect();
        let more = more
            .map(|href| format!(r#"<tr><td><a class="morelink" href="{href}">More</a></td></tr>"#))
            .unwrap_or_default();
        ResponseTemplate::new(200).set_body_string(format!(
            r#"<html op="favorites"><body><table>{rows}{more}</table></body></html>"#
        ))
    }

    /// Mounts two favorites streams of two pages each, expecting each page `hits` times:
    /// stories 1, 2 then 3, comments 11 then 12, 13. Every item exists on Algolia; story 2
    /// links to the article `https://example.com/two`, the rest are text posts.
    async fn two_page_favorites(hn: &MockServer, algolia: &MockServer, hits: [u64; 4]) {
        let streams = [
            (
                false,
                [
                    (&[1, 2][..], Some("favorites?id=psophis&amp;p=2")),
                    (&[3][..], None),
                ],
            ),
            (
                true,
                [
                    (
                        &[11][..],
                        Some("favorites?id=psophis&amp;comments=t&amp;p=2"),
                    ),
                    (&[12, 13][..], None),
                ],
            ),
        ];
        let mut hits = hits.into_iter();
        for (comments, pages) in streams {
            for (page, (ids, more)) in pages.into_iter().enumerate() {
                let stream = match comments {
                    true => Mock::given(query_param("comments", "t")),
                    false => Mock::given(query_param_is_missing("comments")),
                };
                let numbered = match page {
                    0 => stream.and(query_param_is_missing("p")),
                    _ => stream.and(query_param("p", "2")),
                };
                numbered
                    .and(method("GET"))
                    .and(path("/favorites"))
                    .respond_with(favorites_page(ids, more))
                    .expect(hits.next().unwrap())
                    .mount(hn)
                    .await;
            }
        }
        let hits: Vec<_> = [1, 2, 3, 11, 12, 13]
            .iter()
            .map(|id| {
                let article = (*id == 2).then_some("https://example.com/two");
                json!({ "objectID": id.to_string(), "title": format!("Item {id}"),
                        "url": article, "author": "alice", "_tags": ["story"] })
            })
            .collect();
        Mock::given(method("GET"))
            .and(path("/api/v1/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "hits": hits })))
            .mount(algolia)
            .await;
    }

    #[tokio::test]
    async fn incremental_fetch_stops_each_stream_at_a_synced_page() {
        let (hn, algolia) = (MockServer::start().await, MockServer::start().await);
        two_page_favorites(&hn, &algolia, [1, 0, 1, 1]).await;
        let client = HackerNewsClient::with_base_urls(
            "psophis".into(),
            HackerNewsConfig::default(),
            hn.uri(),
            algolia.uri(),
        );
        let existing: Vec<Bookmark> = [
            ("https://news.ycombinator.com/item?id=1", ""),
            ("https://example.com/two", ""),
            ("https://news.ycombinator.com/item?id=12", ""),
            ("https://news.ycombinator.com/item?id=13", ""),
        ]
        .into_iter()
        .map(|(url, note)| Bookmark {
            url: Url::parse(url).unwrap(),
            title: String::new(),
            note: note.into(),
            tags: Vec::new(),
            timestamp: None,
            public: false,
            read_later: false,
        })
        .collect();

        let drafts = crate::http::recording_waits(client.fetch_incremental(&existing))
            .await
            .0
            .unwrap();

        let fetched: Vec<String> = drafts.iter().map(|d| d.bookmark.url.to_string()).collect();
        let permalink = |id| format!("https://news.ycombinator.com/item?id={id}");
        assert_eq!(
            fetched,
            [
                permalink(1),
                "https://example.com/two".into(),
                permalink(11),
                permalink(12),
                permalink(13),
            ],
            "stories stop after page 1, whose article story was saved by hand; comments \
             page 1 has a new favorite, so page 2 is read"
        );
    }

    #[tokio::test]
    async fn incremental_fetch_reads_every_page_when_nothing_is_synced() {
        let (hn, algolia) = (MockServer::start().await, MockServer::start().await);
        two_page_favorites(&hn, &algolia, [1, 1, 1, 1]).await;
        let client = HackerNewsClient::with_base_urls(
            "psophis".into(),
            HackerNewsConfig::default(),
            hn.uri(),
            algolia.uri(),
        );

        let (drafts, waits) = crate::http::recording_waits(client.fetch_incremental(&[])).await;

        assert_eq!(drafts.unwrap().len(), 6);
        assert_eq!(
            waits, [FAVORITES_PAGE_PAUSE; 3],
            "one pause before each of the three pages after the first, none after the last"
        );
    }

    #[tokio::test]
    async fn backup_reads_every_favorites_page() {
        let (hn, algolia) = (MockServer::start().await, MockServer::start().await);
        two_page_favorites(&hn, &algolia, [1, 1, 1, 1]).await;
        let client = HackerNewsClient::with_base_urls(
            "psophis".into(),
            HackerNewsConfig::default(),
            hn.uri(),
            algolia.uri(),
        );

        let dump = crate::http::recording_waits(client.dump()).await.0.unwrap();

        let pages = |stream: &str| dump.raw.iter().filter(|p| p.stream == stream).count();
        assert_eq!(pages("favorites"), 2);
        assert_eq!(pages("favorites-comments"), 2);
        assert_eq!(pages(""), 1, "one batched Algolia lookup, not one per page");
    }

    #[tokio::test]
    async fn a_favorites_429_without_retry_after_is_not_retried() {
        let hn = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .respond_with(ResponseTemplate::new(429))
            .expect(1)
            .mount(&hn)
            .await;

        let err = favorites_client(&hn).fetch().await.unwrap_err();

        let SourceError::RateLimited(message) = err else {
            panic!("expected RateLimited, got {err:?}");
        };
        assert!(message.contains("hn favorites"), "{message}");
        assert!(message.contains("429"), "{message}");
    }

    #[tokio::test]
    async fn a_favorites_429_with_a_long_retry_after_is_not_retried() {
        let hn = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "3600"))
            .expect(1)
            .mount(&hn)
            .await;

        let err = favorites_client(&hn).fetch().await.unwrap_err();

        let SourceError::RateLimited(message) = err else {
            panic!("expected RateLimited, got {err:?}");
        };
        assert!(message.contains("Retry-After: 3600"), "{message}");
    }

    #[tokio::test]
    async fn a_favorites_429_with_a_short_retry_after_is_retried_once() {
        let hn = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .and(query_param_is_missing("comments"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "30"))
            .up_to_n_times(1)
            .expect(1)
            .mount(&hn)
            .await;
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<html op="favorites"><body><table id="hnmain"></table></body></html>"#,
            ))
            .expect(2)
            .mount(&hn)
            .await;

        let (drafts, waits) = crate::http::recording_waits(favorites_client(&hn).fetch()).await;

        assert!(drafts.unwrap().is_empty());
        assert_eq!(
            waits,
            [Duration::from_secs(30), FAVORITES_PAGE_PAUSE],
            "the retry waits out the Retry-After, not the linear backoff, then pauses once \
             before the comments page, and not after it"
        );
    }

    #[tokio::test]
    async fn a_second_favorites_429_stops_the_fetch() {
        let hn = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "1"))
            .expect(2)
            .mount(&hn)
            .await;

        let (result, waits) = crate::http::recording_waits(favorites_client(&hn).fetch()).await;

        let err = result.unwrap_err();
        assert!(matches!(err, SourceError::RateLimited(_)), "got {err:?}");
        assert_eq!(waits, [Duration::from_secs(1)]);
    }

    #[tokio::test]
    async fn collect_ids_stops_at_the_page_cap() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        use wiremock::{Request, Respond};

        // Every page advertises a fresh "More" link, so only the page cap can stop
        // the walk. `recording_waits` skips the inter-page pauses.
        struct PagingFavorites {
            hits: Arc<AtomicUsize>,
        }
        impl Respond for PagingFavorites {
            fn respond(&self, _req: &Request) -> ResponseTemplate {
                let n = self.hits.fetch_add(1, Ordering::SeqCst);
                let body = format!(
                    r#"<html op="favorites"><table>
                         <tr class="athing" id="{n}"><td>x</td></tr>
                         <tr><td><a class="morelink" href="favorites?id=u&amp;p={next}">More</a></td></tr>
                       </table></html>"#,
                    next = n + 2,
                );
                ResponseTemplate::new(200).set_body_string(body)
            }
        }

        let hits = Arc::new(AtomicUsize::new(0));
        let hn = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .respond_with(PagingFavorites { hits: hits.clone() })
            .mount(&hn)
            .await;

        let client = HackerNewsClient::with_base_urls(
            "u".into(),
            HackerNewsConfig::default(),
            hn.uri(),
            "http://unused.invalid".into(),
        );
        let mut ids = Vec::new();
        crate::http::recording_waits(client.collect_ids(
            false,
            None,
            &mut ids,
            &mut HashMap::new(),
            &mut RawSink::disabled(),
        ))
        .await
        .0
        .unwrap();
        assert_eq!(hits.load(Ordering::SeqCst), MAX_FAVORITES_PAGES);
        assert_eq!(ids.len(), MAX_FAVORITES_PAGES);
    }

    #[tokio::test]
    async fn fetch_items_pauses_only_between_algolia_chunks() {
        let algolia = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "hits": [] })))
            .expect(2)
            .mount(&algolia)
            .await;
        let client = HackerNewsClient::with_base_urls(
            "u".into(),
            HackerNewsConfig::default(),
            "http://unused.invalid".into(),
            algolia.uri(),
        );
        let ids: Vec<HackerNewsItemId> = (0..=ITEM_BATCH as u64)
            .map(HackerNewsItemId::from)
            .collect();

        let (items, waits) =
            crate::http::recording_waits(client.fetch_items(&ids, &mut RawSink::disabled())).await;

        assert!(items.unwrap().is_empty());
        assert_eq!(waits, [ALGOLIA_CHUNK_PAUSE]);
    }

    #[tokio::test]
    async fn dump_captures_both_scraped_pages_and_the_algolia_response() {
        let hn = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .and(query_param_is_missing("comments"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<table><tr class="athing" id="42"><td>story</td></tr></table>"#,
            ))
            .mount(&hn)
            .await;
        Mock::given(method("GET"))
            .and(path("/favorites"))
            .and(query_param("comments", "t"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<table><tr class="athing comtr" id="9"><td>comment</td></tr></table>"#,
            ))
            .mount(&hn)
            .await;

        let algolia = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "hits": [
                    { "objectID": "42", "title": "Cool", "url": "https://example.com/x",
                      "author": "alice", "_tags": ["story", "author_alice", "story_42"],
                      // `HackerNewsItem` keeps none of these.
                      "points": 271, "num_comments": 88 },
                    { "objectID": "9", "comment_text": "hi", "author": "carol",
                      "_tags": ["comment", "author_carol", "story_8"] }
                ]
            })))
            .mount(&algolia)
            .await;

        let client = HackerNewsClient::with_base_urls(
            "psophis".into(),
            HackerNewsConfig::default(),
            hn.uri(),
            algolia.uri(),
        );
        let dump = crate::http::recording_waits(client.dump()).await.0.unwrap();

        // Three streams' worth of pages: two scraped HTML pages and one Algolia response.
        let by_stream = |s: &str| -> Vec<&crate::backup::RawPage> {
            dump.raw.iter().filter(|p| p.stream == s).collect()
        };
        assert_eq!(by_stream("favorites").len(), 1);
        assert_eq!(by_stream("favorites-comments").len(), 1);
        assert_eq!(by_stream("").len(), 1);
        assert!(by_stream("favorites")[0].body.contains(r#"id="42""#));
        assert_eq!(by_stream("favorites")[0].kind, RawKind::Html);

        // The Algolia fields the typed item drops are still in the captured body.
        let raw: serde_json::Value = serde_json::from_str(&by_stream("")[0].body).unwrap();
        assert_eq!(raw["hits"][0]["points"], 271);
        assert_eq!(raw["hits"][0]["num_comments"], 88);

        // Each stream becomes its own file; only the HTML ones are non-JSON.
        let files = crate::backup::layout("hackernews-psophis", &dump).unwrap();
        let paths: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        assert_eq!(
            paths,
            vec![
                "raw/hackernews-psophis.json",
                "raw/hackernews-psophis-favorites.html",
                "raw/hackernews-psophis-favorites-comments.html",
                "normalized/hackernews-psophis.json",
            ]
        );

        // End to end: fetch through the driver and land the snapshot on disk.
        let scratch = crate::test_support::scratch_dir("hn-backup-e2e");
        let dir = scratch.path();
        crate::http::recording_waits(crate::backup::run_job(
            &client,
            "hackernews-psophis",
            dir,
            false,
        ))
        .await
        .0
        .unwrap();

        let raw_json = std::fs::read_to_string(dir.join("raw/hackernews-psophis.json")).unwrap();
        let raw: serde_json::Value = serde_json::from_str(&raw_json).unwrap();
        assert_eq!(
            raw[0]["hits"][0]["points"], 271,
            "a dropped Algolia field survives all the way to disk"
        );

        let normalized =
            std::fs::read_to_string(dir.join("normalized/hackernews-psophis.json")).unwrap();
        let records: serde_json::Value = serde_json::from_str(&normalized).unwrap();
        assert_eq!(records[0]["title"], "Cool");
        assert!(records[0]["dedup_key"].is_string());

        let html =
            std::fs::read_to_string(dir.join("raw/hackernews-psophis-favorites.html")).unwrap();
        assert!(html.contains(r#"id="42""#));
    }

    async fn algolia_answering_with_a_long_error_page() -> (MockServer, HackerNewsClient) {
        let algolia = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search"))
            .respond_with(
                ResponseTemplate::new(400).set_body_string(crate::test_support::long_error_page()),
            )
            .mount(&algolia)
            .await;
        let client = HackerNewsClient::with_base_urls(
            "psophis".into(),
            HackerNewsConfig::default(),
            "http://unused.invalid".into(),
            algolia.uri(),
        );
        (algolia, client)
    }

    #[tokio::test]
    async fn algolia_item_error_quotes_a_capped_one_line_excerpt() {
        let (_algolia, client) = algolia_answering_with_a_long_error_page().await;
        let err = client
            .fetch_items(&[HackerNewsItemId::from(1)], &mut RawSink::disabled())
            .await
            .unwrap_err();
        crate::test_support::assert_quotes_capped_excerpt(
            &err.to_string(),
            "hn algolia returned 400 Bad Request",
        );
    }

    #[tokio::test]
    async fn algolia_url_search_error_quotes_a_capped_one_line_excerpt() {
        let (_algolia, client) = algolia_answering_with_a_long_error_page().await;
        let err = client
            .search_by_url(&Url::parse("https://example.com/a").unwrap())
            .await
            .unwrap_err();
        crate::test_support::assert_quotes_capped_excerpt(
            &err.to_string(),
            "hn algolia url search returned 400 Bad Request",
        );
    }
}
