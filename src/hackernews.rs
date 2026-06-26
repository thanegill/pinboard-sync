//! The HackerNews source: a user's public favorites. Scrapes the favorites pages
//! (`/favorites?id=<user>` and `&comments=t`) for item IDs, then batch-reads their
//! details from the Algolia HN search API. Favorited *stories* bookmark the linked
//! article (with the HN discussion in the notes); favorited *comments* bookmark the
//! HN permalink.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use log::warn;
use scraper::{Html, Selector};
use serde::Deserialize;

use crate::bookmark::{Bookmark, BookmarkStore};
use crate::cleanup_pass::{run_pass, CleanupPass, DateOpts};
use crate::htmltext::{blockquote, html_to_markdown, html_to_plain};
use crate::http::send_retrying;
use crate::source::{
    extend_unique, push_prefixed, push_tag, push_tags, url_key, BookmarkDraft, Source, SourceError,
    UrlKey,
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

/// Tag vocabulary for HackerNews favorites. `tags` are applied to every bookmark
/// (defaulting to `["hackernews"]`); the rest default to their built-in values, and
/// an empty string disables that tag.
#[derive(Debug, Clone)]
pub struct HackernewsConfig {
    pub tags: Vec<String>,
    pub comment: String,
    pub author_prefix: String,
    /// Prefix for the tag derived from a `Show/Ask/Tell/Launch HN:` title.
    pub special_prefix: String,
    /// Marker tag for `cleanup --link-discussions`: bookmarks carrying it are
    /// looked up on HN by URL and linked to their discussion.
    pub link_tag: String,
}

impl Default for HackernewsConfig {
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
    kind: String,
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
        let kind = ["comment", "poll", "job", "story"]
            .into_iter()
            .find(|k| h.tags.iter().any(|t| t == k))
            .unwrap_or("story")
            .to_string();
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
    fn into_draft(self, cfg: &HackernewsConfig) -> Option<BookmarkDraft> {
        let item_id = HackerNewsItemId::from(self.id);
        // The HN discussion permalink (always valid — the id sits in the query string).
        let hn_url = Url::from(&item_id);
        let is_comment = self.kind == "comment";

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
                dedup_key: format!("hn:{item_id}"),
            });
        }

        let article = self.url.filter(|s| !s.is_empty());
        let (url, dedup_key) = match article.as_deref() {
            Some(u) => match Url::parse(u) {
                Ok(parsed) => {
                    let key = url_key(&parsed).unwrap_or_else(|| parsed.to_string());
                    (parsed, key)
                }
                Err(e) => {
                    warn!("skipping HN story with unparseable article URL {u}: {e}");
                    return None;
                }
            },
            None => (hn_url.clone(), format!("hn:{item_id}")),
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
    http: reqwest::Client,
    username: String,
    config: HackernewsConfig,
    /// HN site base (overridden in tests).
    base: String,
    /// Algolia API base (overridden in tests).
    algolia: String,
}

impl HackerNewsClient {
    pub fn new(username: String, config: HackernewsConfig) -> anyhow::Result<Self> {
        Self::build(
            username,
            config,
            HN_BASE.to_string(),
            ALGOLIA_BASE.to_string(),
        )
    }

    fn build(
        username: String,
        config: HackernewsConfig,
        base: String,
        algolia: String,
    ) -> anyhow::Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent(BROWSER_UA)
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
            username,
            config,
            base,
            algolia,
        })
    }

    /// Collect favorited item IDs from the stories page (`comments=false`) or the
    /// comments page (`comments=true`), following the "More" link across pages.
    async fn collect_ids(
        &self,
        comments: bool,
        out: &mut Vec<HackerNewsItemId>,
    ) -> Result<(), SourceError> {
        let suffix = if comments { "&comments=t" } else { "" };
        let mut next = Some(format!(
            "{}/favorites?id={}{}",
            self.base, self.username, suffix
        ));
        while let Some(url) = next.take() {
            let resp = send_retrying("hn favorites", MAX_RETRIES, RETRY_DELAY, || {
                self.http.get(&url)
            })
            .await?;
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                return Err(anyhow::anyhow!(
                    "hn favorites for '{}' returned {status}",
                    self.username
                )
                .into());
            }
            let (ids, more) = parse_favorite_ids(&body);
            out.extend(ids.into_iter().map(HackerNewsItemId::from));
            next = more.map(|href| self.resolve(&href));
            // Be gentle between page fetches.
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        Ok(())
    }

    /// Resolve an HN "More" href (typically relative, e.g. `favorites?id=u&p=2`).
    fn resolve(&self, href: &str) -> String {
        if href.starts_with("http") {
            href.to_string()
        } else if let Some(rest) = href.strip_prefix('/') {
            format!("{}/{}", self.base, rest)
        } else {
            format!("{}/{}", self.base, href)
        }
    }

    /// Batch-fetch item details by ID from the Algolia HN search API, keyed by ID.
    /// IDs are queried in chunks of [`ITEM_BATCH`] via `objectID:… OR …`; items that
    /// no longer exist are simply absent from the map.
    async fn fetch_items(
        &self,
        ids: &[HackerNewsItemId],
    ) -> Result<HashMap<HackerNewsItemId, HackerNewsItem>, SourceError> {
        let endpoint = format!("{}/api/v1/search", self.algolia);
        let mut out = HashMap::new();
        for chunk in ids.chunks(ITEM_BATCH) {
            let filters = chunk
                .iter()
                .map(|id| format!("objectID:{id}"))
                .collect::<Vec<_>>()
                .join(" OR ");
            let hits_per_page = chunk.len().to_string();
            let resp = send_retrying("hn algolia", MAX_RETRIES, RETRY_DELAY, || {
                self.http.get(&endpoint).query(&[
                    ("filters", filters.as_str()),
                    ("hitsPerPage", hits_per_page.as_str()),
                ])
            })
            .await?;
            let status = resp.status();
            if !status.is_success() {
                let body = resp.text().await.unwrap_or_default();
                return Err(
                    anyhow::anyhow!("hn algolia returned {status}: {}", body.trim()).into(),
                );
            }
            let search: AlgoliaSearchResponse =
                resp.json().await.context("parsing hn algolia response")?;
            for hit in search.hits {
                let item = HackerNewsItem::from(hit);
                out.insert(HackerNewsItemId::from(item.id), item);
            }
            // Be gentle between chunk queries.
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        Ok(out)
    }

    /// Find the HN story id discussing `url`, via the Algolia URL search. Returns
    /// the id only when a hit's URL matches `url` (so loose matches are ignored).
    async fn search_by_url(&self, url: &Url) -> Result<Option<String>, SourceError> {
        let endpoint = format!("{}/api/v1/search", self.algolia);
        let resp = send_retrying("hn algolia url", MAX_RETRIES, RETRY_DELAY, || {
            self.http.get(&endpoint).query(&[
                ("restrictSearchableAttributes", "url"),
                ("query", url.as_str()),
                ("hitsPerPage", "5"),
            ])
        })
        .await?;
        let status = resp.status();
        if !status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(anyhow::anyhow!(
                "hn algolia url search returned {status}: {}",
                body.trim()
            )
            .into());
        }
        let search: AlgoliaSearchResponse =
            resp.json().await.context("parsing hn algolia response")?;
        let want = url_key(url);
        Ok(search.hits.into_iter().find_map(|h| {
            let hit_key = h
                .url
                .as_deref()
                .and_then(|s| Url::parse(s).ok())
                .and_then(|u| url_key(&u));
            (want.is_some() && hit_key == want).then_some(h.object_id)
        }))
    }
}

impl Source for HackerNewsClient {
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError> {
        let mut ids: Vec<HackerNewsItemId> = Vec::new();
        self.collect_ids(false, &mut ids).await?;
        self.collect_ids(true, &mut ids).await?;
        // De-dup IDs while preserving order (newest first).
        let mut seen = HashSet::new();
        ids.retain(|id| seen.insert(id.clone()));

        let items = self.fetch_items(&ids).await?;
        Ok(ids
            .iter()
            .filter_map(|id| items.get(id).cloned())
            .filter_map(|item| item.into_draft(&self.config))
            .collect())
    }
}

impl UrlKey for HackerNewsClient {
    /// A favorited HN *item* keys on its id (`hn:<id>`); an article bookmark falls back
    /// to the generic host+path key.
    fn dedup_key(&self, url: &Url) -> Option<String> {
        HackerNewsItemId::try_from(url)
            .ok()
            .map(|id| format!("hn:{id}"))
            .or_else(|| url_key(url))
    }
}

/// Options for `cleanup hackernews`.
pub struct HackerNewsCleanupOpts {
    pub dry_run: bool,
    /// Also link `link_tag`-tagged article bookmarks to their HN discussion.
    pub link_discussions: bool,
    /// Re-date bookmarks to the source item's creation time (within the age cap).
    pub use_post_date: bool,
    /// Backdate age cap, in days.
    pub max_age_days: u64,
    /// Re-date items older than the cap to "now" instead of leaving them.
    pub cleanup_stale_to_now: bool,
}

impl HackerNewsCleanupOpts {
    fn date_opts(&self) -> DateOpts {
        DateOpts {
            use_post_date: self.use_post_date,
            max_age_days: self.max_age_days,
            stale_to_now: self.cleanup_stale_to_now,
        }
    }
}

impl HackerNewsClient {
    /// Client for `cleanup hackernews`: only the Algolia API is used (no favorites
    /// scraping), so no username is needed.
    pub fn for_cleanup(config: HackernewsConfig) -> anyhow::Result<Self> {
        Self::build(
            String::new(),
            config,
            HN_BASE.to_string(),
            ALGOLIA_BASE.to_string(),
        )
    }

    /// Normalize existing `news.ycombinator.com/item?id=*` bookmarks: re-fetch each
    /// item and re-shape it (stories rewrite to the article URL, deleting the old HN
    /// URL; comments/text posts update in place), preserving existing tags and the
    /// creation time.
    pub async fn cleanup<P: BookmarkStore>(
        &self,
        pinboard: &P,
        opts: &HackerNewsCleanupOpts,
        bookmarks: &[Bookmark],
    ) -> Result<()> {
        let hackernews_bookmarks: Vec<_> = bookmarks
            .iter()
            .filter(|bookmark| HackerNewsItemId::try_from(&bookmark.url).is_ok())
            .cloned()
            .collect();

        // Batch-fetch every referenced item once.
        let ids: Vec<HackerNewsItemId> = hackernews_bookmarks
            .iter()
            .filter_map(|bookmark| HackerNewsItemId::try_from(&bookmark.url).ok())
            .collect();
        let items = self
            .fetch_items(&ids)
            .await
            .map_err(SourceError::into_anyhow)?;

        let pass = HackerNewsCleanupPass {
            items,
            config: &self.config,
        };
        let mut failed = run_pass(
            pinboard,
            &hackernews_bookmarks,
            opts.dry_run,
            "HN",
            opts.date_opts(),
            &pass,
        )
        .await;

        if opts.link_discussions {
            failed += self.link_discussions(pinboard, opts, bookmarks).await;
        }
        if failed > 0 {
            bail!("{failed} bookmark(s) failed to update");
        }
        Ok(())
    }

    /// For each article bookmark tagged `link_tag`, look it up on HN by URL and, if it
    /// has a discussion, add `HN Link: <discussion>` to the notes and swap the marker
    /// tag for the base HN tags (update in place). Default-off (opt-in via
    /// `--link-discussions`) because it issues one Algolia query per tagged bookmark.
    /// Returns the number of bookmarks that failed to link (logged and skipped).
    async fn link_discussions<P: BookmarkStore>(
        &self,
        pinboard: &P,
        opts: &HackerNewsCleanupOpts,
        bookmarks: &[Bookmark],
    ) -> usize {
        let candidates: Vec<_> = bookmarks
            .iter()
            .filter(|bookmark| {
                HackerNewsItemId::try_from(&bookmark.url).is_err()
                    && bookmark.tags.contains(&self.config.link_tag)
            })
            .cloned()
            .collect();
        run_pass(
            pinboard,
            &candidates,
            opts.dry_run,
            "HN discussion",
            opts.date_opts(),
            &HackerNewsLinkPass { client: self },
        )
        .await
    }
}

/// Re-shapes one favorited HN item bookmark: re-fetch via Algolia and re-derive the
/// draft (stories rewrite to the article URL; comments/text posts update in place),
/// preserving existing tags.
struct HackerNewsCleanupPass<'a> {
    items: HashMap<HackerNewsItemId, HackerNewsItem>,
    config: &'a HackernewsConfig,
}

impl CleanupPass for HackerNewsCleanupPass<'_> {
    async fn plan(&self, bookmark: &Bookmark) -> Result<Option<Bookmark>> {
        // The pass is filtered to HN item URLs, so this matches.
        let id = HackerNewsItemId::try_from(&bookmark.url).ok();
        let Some(item) = id.and_then(|id| self.items.get(&id)) else {
            return Ok(None);
        };
        // Re-derive the bookmark from the fresh item (url/title/note/date), then preserve
        // the stored bookmark's existing tags and privacy on the re-write.
        let Some(mut new) = item.clone().into_draft(self.config).map(|d| d.bookmark) else {
            return Ok(None);
        };
        let mut tags = bookmark.tags.clone();
        extend_unique(&mut tags, &new.tags);
        new.tags = tags;
        new.public = bookmark.public;
        new.read_later = bookmark.read_later;
        Ok(Some(new))
    }
}

/// Links one `link_tag`-tagged article bookmark to its HN discussion: look it up by
/// URL, add an `HN Link:` line to the notes, and swap the marker tag for the base HN
/// tags. Always in-place — the URL is unchanged and `src_date` is `None`, so the
/// driver preserves the stored date regardless of the dating policy.
struct HackerNewsLinkPass<'a> {
    client: &'a HackerNewsClient,
}

impl CleanupPass for HackerNewsLinkPass<'_> {
    async fn plan(&self, bookmark: &Bookmark) -> Result<Option<Bookmark>> {
        let id = match self.client.search_by_url(&bookmark.url).await {
            Ok(Some(id)) => id,
            Ok(None) => return Ok(None),
            Err(e) => return Err(SourceError::into_anyhow(e)),
        };

        let hn_link = format!("HN Link: https://news.ycombinator.com/item?id={id}");
        let note = if bookmark.note.contains("HN Link:") {
            bookmark.note.clone()
        } else if bookmark.note.is_empty() {
            hn_link
        } else {
            format!("{}\n\n{hn_link}", bookmark.note)
        };

        // Clean the title (arbitrary article bookmarks may carry HTML entities).
        let title = html_to_plain(&bookmark.title);

        // Drop the marker tag, add the base HN tags.
        let mut tags: Vec<String> = bookmark
            .tags
            .iter()
            .filter(|t| **t != self.client.config.link_tag)
            .cloned()
            .collect();
        extend_unique(&mut tags, &self.client.config.tags);

        Ok(Some(Bookmark {
            url: bookmark.url.clone(),
            title,
            note,
            tags,
            // No candidate source time — the driver preserves the stored time.
            timestamp: None,
            public: bookmark.public,
            read_later: bookmark.read_later,
        }))
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

/// Parse a favorites page: the favorited item IDs (the `id` of each `tr.athing`
/// row) and the "More" link href, if any. Pure, so it's unit-tested on sample HTML.
fn parse_favorite_ids(html: &str) -> (Vec<String>, Option<String>) {
    let doc = Html::parse_document(html);
    let row = Selector::parse("tr.athing").unwrap();
    let ids = doc
        .select(&row)
        .filter_map(|e| e.value().attr("id").map(str::to_string))
        .collect();
    let more = Selector::parse("a.morelink").unwrap();
    let next = doc
        .select(&more)
        .next()
        .and_then(|e| e.value().attr("href").map(str::to_string));
    (ids, next)
}

/// The slug for a `Show/Ask/Tell/Launch HN:` title prefix (e.g. `Show HN:` →
/// `show-hn`), or `None` for ordinary titles.
fn special_type(title: &str) -> Option<String> {
    for kind in ["Show HN", "Ask HN", "Tell HN", "Launch HN"] {
        if title.starts_with(kind) && title[kind.len()..].starts_with(':') {
            return Some(kind.to_lowercase().replace(' ', "-"));
        }
    }
    None
}

#[cfg(test)]
impl HackerNewsClient {
    fn with_base_urls(
        username: String,
        config: HackernewsConfig,
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

    #[test]
    fn story_with_url_bookmarks_the_article_with_hn_link_in_notes() {
        let d = item(json!({
            "id": 42, "type": "story", "by": "alice",
            "title": "Cool thing", "url": "https://example.com/x"
        }))
        .into_draft(&HackernewsConfig::default())
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
    fn text_post_without_url_bookmarks_the_hn_permalink() {
        let d = item(json!({
            "id": 7, "type": "story", "by": "bob",
            "title": "Ask HN: How?", "text": "<p>details</p>"
        }))
        .into_draft(&HackernewsConfig::default())
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
        .into_draft(&HackernewsConfig::default())
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
        .into_draft(&HackernewsConfig::default())
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
        .into_draft(&HackernewsConfig::default())
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
                .into_draft(&HackernewsConfig::default())
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
        let (ids, more) = parse_favorite_ids(html);
        assert_eq!(ids, vec!["111", "222"]);
        assert_eq!(more.as_deref(), Some("favorites?id=u&p=2"));
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
        let c = HackerNewsClient::new("u".into(), HackernewsConfig::default()).unwrap();
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
            HackernewsConfig::default(),
            hn.uri(),
            algolia.uri(),
        );
        let drafts = client.fetch().await.unwrap();
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
    async fn cleanup_rewrites_story_url_and_deletes_old() {
        use crate::pinboard::PinboardBookmark;
        use crate::test_support::FakePinboard;

        let algolia = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "hits": [
                    { "objectID": "42", "title": "Cool", "url": "https://example.com/x",
                      "author": "alice", "_tags": ["story", "author_alice", "story_42"] }
                ]
            })))
            .mount(&algolia)
            .await;

        let pinboard = FakePinboard {
            all: vec![PinboardBookmark {
                url: "https://news.ycombinator.com/item?id=42".into(),
                description: "old title".into(),
                extended: String::new(),
                tags: "hackernews mine".into(),
                time: "2020-01-01T00:00:00Z".into(),
                shared: "no".into(),
                toread: "no".into(),
            }
            .try_into()
            .unwrap()],
            ..Default::default()
        };

        let client = HackerNewsClient::with_base_urls(
            String::new(),
            HackernewsConfig::default(),
            "unused".into(),
            algolia.uri(),
        );
        let bookmarks = pinboard.all.clone();
        client
            .cleanup(
                &pinboard,
                &HackerNewsCleanupOpts {
                    dry_run: false,
                    link_discussions: false,
                    use_post_date: false,
                    max_age_days: 30,
                    cleanup_stale_to_now: false,
                },
                &bookmarks,
            )
            .await
            .unwrap();

        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].url, "https://example.com/x");
        assert_eq!(updated[0].description, "Cool");
        // Existing tags are preserved and augmented.
        assert!(updated[0].tags.contains(&"mine".to_string()));
        // The old HN item URL is deleted after the rewrite.
        assert_eq!(
            pinboard.deleted.borrow().as_slice(),
            &["https://news.ycombinator.com/item?id=42".to_string()]
        );
    }

    #[tokio::test]
    async fn cleanup_strips_redundant_hn_link_from_text_post_notes() {
        use crate::pinboard::PinboardBookmark;
        use crate::test_support::FakePinboard;

        // A text post (no url): the bookmark URL already is the HN permalink.
        let algolia = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "hits": [
                    { "objectID": "7", "title": "Ask HN: How?", "story_text": "<p>details</p>",
                      "author": "bob", "_tags": ["story", "author_bob", "story_7"] }
                ]
            })))
            .mount(&algolia)
            .await;

        let pinboard = FakePinboard {
            // Notes carry the old redundant self-link to the same item.
            all: vec![PinboardBookmark {
                url: "https://news.ycombinator.com/item?id=7".into(),
                description: "Ask HN: How?".into(),
                extended: "HN Link: https://news.ycombinator.com/item?id=7\n\n<blockquote><p>details</p></blockquote>".into(),
                tags: "hackernews".into(),
                time: "2020-01-01T00:00:00Z".into(),
                shared: "no".into(),
                toread: "no".into(),
            }
            .try_into().unwrap()],
            ..Default::default()
        };

        let client = HackerNewsClient::with_base_urls(
            String::new(),
            HackernewsConfig::default(),
            "unused".into(),
            algolia.uri(),
        );
        let bookmarks = pinboard.all.clone();
        client
            .cleanup(
                &pinboard,
                &HackerNewsCleanupOpts {
                    dry_run: false,
                    link_discussions: false,
                    use_post_date: false,
                    max_age_days: 30,
                    cleanup_stale_to_now: false,
                },
                &bookmarks,
            )
            .await
            .unwrap();

        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        // URL unchanged (in-place); the redundant HN Link line is gone, leaving the text
        // with its inner HTML converted to Markdown.
        assert_eq!(updated[0].url, "https://news.ycombinator.com/item?id=7");
        assert_eq!(updated[0].extended, "<blockquote>details</blockquote>");
        assert!(pinboard.deleted.borrow().is_empty());
    }

    #[tokio::test]
    async fn link_discussions_adds_hn_link_to_tagged_article() {
        use crate::pinboard::PinboardBookmark;
        use crate::test_support::FakePinboard;

        // The URL search returns a matching HN story id; the item batch (unused
        // here) would hit the same path, so the matcher keys on restrictSearch...
        let algolia = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/search"))
            .and(query_param("restrictSearchableAttributes", "url"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "hits": [
                    { "objectID": "99", "url": "https://example.com/x",
                      "_tags": ["story", "author_a", "story_99"] }
                ]
            })))
            .mount(&algolia)
            .await;

        let pinboard = FakePinboard {
            all: vec![PinboardBookmark {
                url: "https://example.com/x".into(),
                description: "An article".into(),
                extended: "my notes".into(),
                tags: "find-hn reading".into(),
                time: "2020-01-01T00:00:00Z".into(),
                shared: "no".into(),
                toread: "no".into(),
            }
            .try_into()
            .unwrap()],
            ..Default::default()
        };

        let client = HackerNewsClient::with_base_urls(
            String::new(),
            HackernewsConfig::default(),
            "unused".into(),
            algolia.uri(),
        );
        let bookmarks = pinboard.all.clone();
        client
            .cleanup(
                &pinboard,
                &HackerNewsCleanupOpts {
                    dry_run: false,
                    link_discussions: true,
                    use_post_date: false,
                    max_age_days: 30,
                    cleanup_stale_to_now: false,
                },
                &bookmarks,
            )
            .await
            .unwrap();

        let updated = pinboard.updated.borrow();
        assert_eq!(updated.len(), 1);
        // URL unchanged (in-place), notes gain the HN link, marker tag swapped for
        // the base HN tag, existing tags kept.
        assert_eq!(updated[0].url, "https://example.com/x");
        assert!(updated[0].tags.contains(&"hackernews".to_string()));
        assert!(!updated[0].tags.contains(&"find-hn".to_string()));
        assert!(updated[0].tags.contains(&"reading".to_string()));
        assert!(pinboard.deleted.borrow().is_empty());
    }
}
