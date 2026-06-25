//! The HackerNews source: a user's public favorites. Scrapes the favorites pages
//! (`/favorites?id=<user>` and `&comments=t`) for item IDs, then batch-reads their
//! details from the Algolia HN search API. Favorited *stories* bookmark the linked
//! article (with the HN discussion in the notes); favorited *comments* bookmark the
//! HN permalink.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use log::{debug, error, info};
use scraper::{Html, Selector};
use serde::Deserialize;

use crate::htmltext::{blockquote, html_to_markdown, html_to_plain};
use crate::http::send_retrying;
use crate::pinboard::{apply_update, Bookmark, BookmarkStore, BookmarkUpdate};
use crate::source::{
    extend_unique, push_prefixed, push_tag, push_tags, split_host_path, tags_differ, url_key,
    BookmarkDraft, Source, SourceError,
};

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
struct Item {
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
struct SearchResponse {
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

impl From<AlgoliaHit> for Item {
    fn from(h: AlgoliaHit) -> Self {
        // The item type is carried in `_tags` (e.g. "story"/"comment"/"poll"/"job").
        let kind = ["comment", "poll", "job", "story"]
            .into_iter()
            .find(|k| h.tags.iter().any(|t| t == k))
            .unwrap_or("story")
            .to_string();
        Item {
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

impl Item {
    /// Shape the item into a Pinboard draft. Comments keep the HN permalink; stories
    /// bookmark the linked article (HN discussion in the notes), falling back to the
    /// HN permalink for text posts (Ask HN, etc.).
    fn into_draft(self, cfg: &HackernewsConfig) -> BookmarkDraft {
        let hn_url = hn_item_url(self.id);
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
            return BookmarkDraft {
                url: hn_url.clone(),
                description: html_to_plain(&format!("HN: Comment by {}", self.by)),
                extended: if md.is_empty() { md } else { blockquote(&md) },
                tags,
                dedup_key: format!("hn:{}", self.id),
                toread: false,
                shared: false,
                post_date: self.created_at,
            };
        }

        let article = self.url.filter(|s| !s.is_empty());
        let (url, dedup_key) = match &article {
            Some(u) => (u.clone(), url_key(u).unwrap_or_else(|| u.clone())),
            None => (hn_url.clone(), format!("hn:{}", self.id)),
        };
        let description = html_to_plain(
            &self
                .title
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("HN: {} by {}", self.kind, self.by)),
        );
        // The `HN Link:` line points at the discussion. For a text post the bookmark
        // URL already *is* that permalink, so including it would duplicate the URL —
        // skip it and let the notes carry just the post text.
        let mut extended = if article.is_some() {
            format!("HN Link: {hn_url}")
        } else {
            String::new()
        };
        if let Some(text) = self.text.filter(|s| !s.is_empty()) {
            // Convert the raw Algolia HTML to Markdown, wrapped in a <blockquote>.
            let block = blockquote(&html_to_markdown(&text));
            extended = if extended.is_empty() {
                block
            } else {
                format!("{extended}\n\n{block}")
            };
        }
        BookmarkDraft {
            url,
            description,
            extended,
            tags,
            dedup_key,
            toread: false,
            shared: false,
            post_date: self.created_at,
        }
    }
}

/// Reads a user's public HackerNews favorites.
pub struct HnClient {
    http: reqwest::Client,
    username: String,
    config: HackernewsConfig,
    /// HN site base (overridden in tests).
    base: String,
    /// Algolia API base (overridden in tests).
    algolia: String,
}

impl HnClient {
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
    async fn collect_ids(&self, comments: bool, out: &mut Vec<String>) -> Result<(), SourceError> {
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
            out.extend(ids);
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
    async fn fetch_items(&self, ids: &[String]) -> Result<HashMap<String, Item>, SourceError> {
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
            let search: SearchResponse =
                resp.json().await.context("parsing hn algolia response")?;
            for hit in search.hits {
                let item = Item::from(hit);
                out.insert(item.id.to_string(), item);
            }
            // Be gentle between chunk queries.
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        Ok(out)
    }

    /// Find the HN story id discussing `url`, via the Algolia URL search. Returns
    /// the id only when a hit's URL matches `url` (so loose matches are ignored).
    async fn search_by_url(&self, url: &str) -> Result<Option<String>, SourceError> {
        let endpoint = format!("{}/api/v1/search", self.algolia);
        let resp = send_retrying("hn algolia url", MAX_RETRIES, RETRY_DELAY, || {
            self.http.get(&endpoint).query(&[
                ("restrictSearchableAttributes", "url"),
                ("query", url),
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
        let search: SearchResponse = resp.json().await.context("parsing hn algolia response")?;
        let want = url_key(url);
        Ok(search.hits.into_iter().find_map(|h| {
            let hit_key = h.url.as_deref().and_then(url_key);
            (want.is_some() && hit_key == want).then_some(h.object_id)
        }))
    }
}

impl Source for HnClient {
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError> {
        let mut ids = Vec::new();
        self.collect_ids(false, &mut ids).await?;
        self.collect_ids(true, &mut ids).await?;
        // De-dup IDs while preserving order (newest first).
        let mut seen = HashSet::new();
        ids.retain(|id| seen.insert(id.clone()));

        let items = self.fetch_items(&ids).await?;
        Ok(ids
            .iter()
            .filter_map(|id| items.get(id).cloned())
            .map(|item| item.into_draft(&self.config))
            .collect())
    }

    fn existing_key(&self, url: &str) -> Option<String> {
        hn_item_id(url)
            .map(|id| format!("hn:{id}"))
            .or_else(|| url_key(url))
    }
}

/// Options for `cleanup hackernews`.
pub struct HnCleanupOpts {
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

impl HnClient {
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
        opts: &HnCleanupOpts,
        bookmarks: &[Bookmark],
    ) -> Result<()> {
        let hn_bms: Vec<_> = bookmarks
            .iter()
            .filter(|b| hn_item_id(&b.url).is_some())
            .cloned()
            .collect();
        info!(
            "scanning {} HN bookmark(s){}",
            hn_bms.len(),
            if opts.dry_run { " (dry run)" } else { "" }
        );

        // Batch-fetch every referenced item once.
        let ids: Vec<String> = hn_bms.iter().filter_map(|b| hn_item_id(&b.url)).collect();
        let items = self
            .fetch_items(&ids)
            .await
            .map_err(SourceError::into_anyhow)?;

        let now = crate::timefmt::now_unix();
        let mut changed = 0usize;
        let mut failed = 0usize;
        let mut wrote = false;
        for bm in &hn_bms {
            let id = hn_item_id(&bm.url).expect("filtered to HN item URLs");
            let Some(item) = items.get(&id) else {
                continue;
            };
            let src_ts = item.created_at;
            let draft = item.clone().into_draft(&self.config);

            // Preserve existing tags, appending any freshly-derived ones.
            let mut tags = bm.tag_list();
            extend_unique(&mut tags, &draft.tags);

            let dt = crate::timefmt::cleanup_dt(
                opts.use_post_date,
                opts.max_age_days,
                opts.cleanup_stale_to_now,
                src_ts,
                now,
                &bm.time,
            );

            let url_changed = draft.url != bm.url;
            let tags_changed = tags != bm.tag_list();
            let desc_changed = draft.description != bm.description;
            let ext_changed = draft.extended != bm.extended;
            let date_changed = dt != bm.time;
            if !(url_changed || tags_changed || desc_changed || ext_changed || date_changed) {
                continue;
            }

            if opts.dry_run {
                changed += 1;
                println!("[dry-run] {}", bm.url);
                if url_changed {
                    println!("          url   -> {}", draft.url);
                }
                if desc_changed {
                    println!("          title -> {}", draft.description);
                }
                if tags_changed {
                    println!("          tags  -> [{}]", tags.join(" "));
                }
                if date_changed {
                    println!("          date  -> {dt}");
                }
                continue;
            }

            // Log and skip a single failed update so the rest of the pass still runs.
            match apply_update(
                pinboard,
                &mut wrote,
                BookmarkUpdate {
                    url: &draft.url,
                    description: &draft.description,
                    extended: &draft.extended,
                    tags: &tags,
                    shared: bm.is_shared(),
                    toread: bm.is_toread(),
                    dt: &dt,
                },
                url_changed.then_some(bm.url.as_str()),
            )
            .await
            {
                Ok(()) => {
                    changed += 1;
                    debug!("updated {} -> {} [{}]", bm.url, draft.url, tags.join(" "));
                }
                Err(e) => {
                    failed += 1;
                    error!("updating bookmark {}: {e:#}", bm.url);
                }
            }
        }

        if opts.dry_run {
            println!("{changed} bookmark(s) would change.");
        } else {
            info!("done: updated {changed} bookmark(s)");
        }

        if opts.link_discussions {
            failed += self.link_discussions(pinboard, opts, bookmarks).await?;
        }
        if failed > 0 {
            bail!("{failed} bookmark(s) failed to update");
        }
        Ok(())
    }

    /// For each article bookmark tagged `link_tag`, look it up on HN by URL and, if
    /// it has a discussion, add `HN Link: <discussion>` to the notes, swap the
    /// marker tag for the base HN tags, and update in place. Default-off (opt-in via
    /// `--link-discussions`) because it issues one Algolia query per tagged bookmark.
    /// Returns the number of bookmarks that failed to link (logged and skipped).
    async fn link_discussions<P: BookmarkStore>(
        &self,
        pinboard: &P,
        opts: &HnCleanupOpts,
        bookmarks: &[Bookmark],
    ) -> Result<usize> {
        let candidates: Vec<_> = bookmarks
            .iter()
            .filter(|b| {
                hn_item_id(&b.url).is_none() && b.tag_list().contains(&self.config.link_tag)
            })
            .cloned()
            .collect();
        info!(
            "linking {} '{}'-tagged bookmark(s) to HN discussions{}",
            candidates.len(),
            self.config.link_tag,
            if opts.dry_run { " (dry run)" } else { "" }
        );

        let mut changed = 0usize;
        let mut failed = 0usize;
        let mut wrote = false;
        for bm in &candidates {
            // Log and skip a single bookmark whose HN lookup fails.
            let id = match self.search_by_url(&bm.url).await {
                Ok(Some(id)) => id,
                Ok(None) => continue,
                Err(e) => {
                    failed += 1;
                    error!("looking up {}: {:#}", bm.url, SourceError::into_anyhow(e));
                    continue;
                }
            };

            let hn_link = format!("HN Link: https://news.ycombinator.com/item?id={id}");
            let extended = if bm.extended.contains("HN Link:") {
                bm.extended.clone()
            } else if bm.extended.is_empty() {
                hn_link.clone()
            } else {
                format!("{}\n\n{hn_link}", bm.extended)
            };

            // Drop the marker tag, add the base HN tags.
            let mut tags: Vec<String> = bm
                .tag_list()
                .into_iter()
                .filter(|t| *t != self.config.link_tag)
                .collect();
            extend_unique(&mut tags, &self.config.tags);

            if extended == bm.extended && !tags_differ(&bm.tag_list(), &tags) {
                continue;
            }

            if opts.dry_run {
                changed += 1;
                println!("[dry-run] {}", bm.url);
                println!("          notes -> {hn_link}");
                println!("          tags  -> [{}]", tags.join(" "));
                continue;
            }

            // Log and skip a single failed update so the rest of the pass still runs.
            match apply_update(
                pinboard,
                &mut wrote,
                BookmarkUpdate {
                    url: &bm.url,
                    description: &bm.description,
                    extended: &extended,
                    tags: &tags,
                    shared: bm.is_shared(),
                    toread: bm.is_toread(),
                    dt: &bm.time,
                },
                None,
            )
            .await
            {
                Ok(()) => {
                    changed += 1;
                    debug!("linked {} -> item?id={id}", bm.url);
                }
                Err(e) => {
                    failed += 1;
                    error!("linking bookmark {}: {e:#}", bm.url);
                }
            }
        }

        if opts.dry_run {
            println!("{changed} bookmark(s) would be linked.");
        } else {
            info!("done: linked {changed} bookmark(s)");
        }
        Ok(failed)
    }
}

/// The HN discussion permalink for an item id.
fn hn_item_url(id: u64) -> String {
    format!("https://news.ycombinator.com/item?id={id}")
}

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

/// Extract the item id from an HN `item?id=<n>` URL (host must be exactly
/// `news.ycombinator.com`), or `None` for non-HN-item URLs.
fn hn_item_id(url: &str) -> Option<String> {
    let (host, path) = split_host_path(url);
    let rest = path.strip_prefix('/').unwrap_or(path);
    if host != "news.ycombinator.com" || !rest.starts_with("item") {
        return None;
    }
    let query = rest.split_once('?')?.1;
    let id: String = query
        .split('&')
        .find_map(|kv| kv.strip_prefix("id="))?
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    (!id.is_empty()).then_some(id)
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
impl HnClient {
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

    fn item(value: serde_json::Value) -> Item {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn story_with_url_bookmarks_the_article_with_hn_link_in_notes() {
        let d = item(json!({
            "id": 42, "type": "story", "by": "alice",
            "title": "Cool thing", "url": "https://example.com/x"
        }))
        .into_draft(&HackernewsConfig::default());
        assert_eq!(d.url, "https://example.com/x");
        assert_eq!(d.description, "Cool thing");
        assert_eq!(
            d.extended,
            "HN Link: https://news.ycombinator.com/item?id=42"
        );
        assert_eq!(d.dedup_key, "example.com/x");
        assert_eq!(d.tags, vec!["hackernews", "author:hackernews:alice"]);
    }

    #[test]
    fn text_post_without_url_bookmarks_the_hn_permalink() {
        let d = item(json!({
            "id": 7, "type": "story", "by": "bob",
            "title": "Ask HN: How?", "text": "<p>details</p>"
        }))
        .into_draft(&HackernewsConfig::default());
        assert_eq!(d.url, "https://news.ycombinator.com/item?id=7");
        assert_eq!(d.dedup_key, "hn:7");
        // The bookmark URL is already the HN permalink, so the notes carry only the post
        // text (no redundant `HN Link:`), with the inner HTML converted to Markdown.
        assert_eq!(d.extended, "<blockquote>details</blockquote>");
        // Ask HN: → special-type tag.
        assert!(d.tags.contains(&"hackernews:ask-hn".to_string()));
    }

    #[test]
    fn text_post_without_text_has_empty_notes() {
        let d = item(json!({
            "id": 8, "type": "story", "by": "bob", "title": "Ask HN: empty?"
        }))
        .into_draft(&HackernewsConfig::default());
        assert_eq!(d.url, "https://news.ycombinator.com/item?id=8");
        // No text and no redundant HN link: nothing to put in the notes.
        assert_eq!(d.extended, "");
    }

    #[test]
    fn html_title_is_plain_and_html_body_becomes_markdown_blockquote() {
        // An article story (has a url), so the notes keep the `HN Link:` discussion line.
        let d = item(json!({
            "id": 11, "type": "story", "by": "dave",
            "title": "Rust&#x27;s &amp; more", "url": "https://example.com/a",
            "text": "<p>see <a href=\"https://x.com\">x</a> &gt; y</p>"
        }))
        .into_draft(&HackernewsConfig::default());
        // Title: entities decoded, tags stripped, single line.
        assert_eq!(d.description, "Rust's & more");
        // Body: HTML converted to Markdown, wrapped in a literal <blockquote>.
        assert_eq!(
            d.extended,
            "HN Link: https://news.ycombinator.com/item?id=11\n\n<blockquote>see [x](https://x.com) > y</blockquote>"
        );
    }

    #[test]
    fn comment_bookmarks_permalink_with_comment_tag() {
        let d = item(json!({
            "id": 9, "type": "comment", "by": "carol", "text": "my reply"
        }))
        .into_draft(&HackernewsConfig::default());
        assert_eq!(d.url, "https://news.ycombinator.com/item?id=9");
        assert_eq!(d.description, "HN: Comment by carol");
        assert_eq!(d.extended, "<blockquote>my reply</blockquote>");
        assert_eq!(d.dedup_key, "hn:9");
        assert_eq!(
            d.tags,
            vec![
                "hackernews",
                "hackernews-comment",
                "author:hackernews:carol"
            ]
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
        assert_eq!(
            hn_item_id("https://news.ycombinator.com/item?id=42").as_deref(),
            Some("42")
        );
        assert_eq!(hn_item_id("https://example.com/item?id=42"), None);
        assert_eq!(hn_item_id("https://news.ycombinator.com/news"), None);
    }

    #[test]
    fn existing_key_distinguishes_hn_items_from_articles() {
        let c = HnClient::new("u".into(), HackernewsConfig::default()).unwrap();
        assert_eq!(
            c.existing_key("https://news.ycombinator.com/item?id=42")
                .as_deref(),
            Some("hn:42")
        );
        assert_eq!(
            c.existing_key("https://example.com/x").as_deref(),
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

        let client = HnClient::with_base_urls(
            "psophis".into(),
            HackernewsConfig::default(),
            hn.uri(),
            algolia.uri(),
        );
        let drafts = client.fetch().await.unwrap();
        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[0].url, "https://example.com/x");
        assert_eq!(drafts[1].url, "https://news.ycombinator.com/item?id=9");
        assert!(drafts[1].tags.contains(&"hackernews-comment".to_string()));
    }

    #[tokio::test]
    async fn cleanup_rewrites_story_url_and_deletes_old() {
        use crate::pinboard::Bookmark;
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
            all: vec![Bookmark {
                url: "https://news.ycombinator.com/item?id=42".into(),
                description: "old title".into(),
                extended: String::new(),
                tags: "hackernews mine".into(),
                time: "2020-01-01T00:00:00Z".into(),
                shared: "no".into(),
                toread: "no".into(),
            }],
            ..Default::default()
        };

        let client = HnClient::with_base_urls(
            String::new(),
            HackernewsConfig::default(),
            "unused".into(),
            algolia.uri(),
        );
        let bookmarks = pinboard.all.clone();
        client
            .cleanup(
                &pinboard,
                &HnCleanupOpts {
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
        use crate::pinboard::Bookmark;
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
            all: vec![Bookmark {
                url: "https://news.ycombinator.com/item?id=7".into(),
                description: "Ask HN: How?".into(),
                extended: "HN Link: https://news.ycombinator.com/item?id=7\n\n<blockquote><p>details</p></blockquote>".into(),
                tags: "hackernews".into(),
                time: "2020-01-01T00:00:00Z".into(),
                shared: "no".into(),
                toread: "no".into(),
            }],
            ..Default::default()
        };

        let client = HnClient::with_base_urls(
            String::new(),
            HackernewsConfig::default(),
            "unused".into(),
            algolia.uri(),
        );
        let bookmarks = pinboard.all.clone();
        client
            .cleanup(
                &pinboard,
                &HnCleanupOpts {
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
        use crate::pinboard::Bookmark;
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
            all: vec![Bookmark {
                url: "https://example.com/x".into(),
                description: "An article".into(),
                extended: "my notes".into(),
                tags: "find-hn reading".into(),
                time: "2020-01-01T00:00:00Z".into(),
                shared: "no".into(),
                toread: "no".into(),
            }],
            ..Default::default()
        };

        let client = HnClient::with_base_urls(
            String::new(),
            HackernewsConfig::default(),
            "unused".into(),
            algolia.uri(),
        );
        let bookmarks = pinboard.all.clone();
        client
            .cleanup(
                &pinboard,
                &HnCleanupOpts {
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
