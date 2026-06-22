//! The HackerNews source: a user's public favorites. Scrapes the favorites pages
//! (`/favorites?id=<user>` and `&comments=t`) for item IDs, then reads each item
//! from the Firebase API for its title/url/text. Favorited *stories* bookmark the
//! linked article (with the HN discussion in the notes); favorited *comments*
//! bookmark the HN permalink.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context;
use scraper::{Html, Selector};
use serde::Deserialize;

use crate::http::send_retrying;
use crate::source::{push_prefixed, push_tag, url_key, BookmarkDraft, Source, SourceError};

/// HN blocks some default User-Agents on the HTML pages, so present a browser one.
const BROWSER_UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0";
const HN_BASE: &str = "https://news.ycombinator.com";
const FIREBASE_BASE: &str = "https://hacker-news.firebaseio.com";
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
}

impl Default for HackernewsConfig {
    fn default() -> Self {
        Self {
            tags: vec!["hackernews".into()],
            comment: "hackernews-comment".into(),
            author_prefix: "author:hackernews:".into(),
            special_prefix: "hackernews:".into(),
        }
    }
}

/// A HackerNews item, as returned by the Firebase API. Missing items decode to
/// `null` → `None`.
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
}

impl Item {
    /// Shape the item into a Pinboard draft. Comments keep the HN permalink; stories
    /// bookmark the linked article (HN discussion in the notes), falling back to the
    /// HN permalink for text posts (Ask HN, etc.).
    fn into_draft(self, cfg: &HackernewsConfig) -> BookmarkDraft {
        let hn_url = hn_item_url(self.id);
        let is_comment = self.kind == "comment";

        let mut tags = Vec::new();
        for tag in &cfg.tags {
            push_tag(&mut tags, tag);
        }
        if is_comment {
            push_tag(&mut tags, &cfg.comment);
        }
        push_prefixed(&mut tags, &cfg.author_prefix, &self.by);
        if let Some(special) = self.title.as_deref().and_then(special_type) {
            push_prefixed(&mut tags, &cfg.special_prefix, &special);
        }

        if is_comment {
            return BookmarkDraft {
                url: hn_url.clone(),
                description: format!("HN: Comment by {}", self.by),
                extended: self.text.unwrap_or_default(),
                tags,
                dedup_key: format!("hn:{}", self.id),
            };
        }

        let article = self.url.filter(|s| !s.is_empty());
        let (url, dedup_key) = match &article {
            Some(u) => (u.clone(), url_key(u).unwrap_or_else(|| u.clone())),
            None => (hn_url.clone(), format!("hn:{}", self.id)),
        };
        let description = self
            .title
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| format!("HN: {} by {}", self.kind, self.by));
        let mut extended = format!("HN Link: {hn_url}");
        if let Some(text) = self.text.filter(|s| !s.is_empty()) {
            extended = format!("{extended}\n\n<blockquote>{text}</blockquote>");
        }
        BookmarkDraft {
            url,
            description,
            extended,
            tags,
            dedup_key,
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
    /// Firebase API base (overridden in tests).
    firebase: String,
}

impl HnClient {
    pub fn new(username: String, config: HackernewsConfig) -> anyhow::Result<Self> {
        Self::build(
            username,
            config,
            HN_BASE.to_string(),
            FIREBASE_BASE.to_string(),
        )
    }

    fn build(
        username: String,
        config: HackernewsConfig,
        base: String,
        firebase: String,
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
            firebase,
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

    /// Fetch one item from the Firebase API (`None` if it no longer exists).
    async fn fetch_item(&self, id: &str) -> Result<Option<Item>, SourceError> {
        let url = format!("{}/v0/item/{}.json", self.firebase, id);
        let resp =
            send_retrying("hn item", MAX_RETRIES, RETRY_DELAY, || self.http.get(&url)).await?;
        let status = resp.status();
        if !status.is_success() {
            return Err(anyhow::anyhow!("hn item {id} returned {status}").into());
        }
        resp.json()
            .await
            .context("parsing hn item")
            .map_err(Into::into)
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

        let mut drafts = Vec::new();
        for id in ids {
            if let Some(item) = self.fetch_item(&id).await? {
                drafts.push(item.into_draft(&self.config));
            }
        }
        Ok(drafts)
    }

    fn existing_key(&self, url: &str) -> Option<String> {
        match hn_item_id(url) {
            Some(id) => Some(format!("hn:{id}")),
            None => url_key(url),
        }
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

/// Extract the item id from an HN `item?id=<n>` URL (any reddit-style host check),
/// or `None` for non-HN-item URLs.
fn hn_item_id(url: &str) -> Option<String> {
    let after = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let (host, rest) = after.split_once('/')?;
    let host = host.split(':').next().unwrap_or(host).to_ascii_lowercase();
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
        firebase: String,
    ) -> Self {
        Self::build(username, config, base, firebase).unwrap()
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
        assert_eq!(
            d.extended,
            "HN Link: https://news.ycombinator.com/item?id=7\n\n<blockquote><p>details</p></blockquote>"
        );
        // Ask HN: → special-type tag.
        assert!(d.tags.contains(&"hackernews:ask-hn".to_string()));
    }

    #[test]
    fn comment_bookmarks_permalink_with_comment_tag() {
        let d = item(json!({
            "id": 9, "type": "comment", "by": "carol", "text": "my reply"
        }))
        .into_draft(&HackernewsConfig::default());
        assert_eq!(d.url, "https://news.ycombinator.com/item?id=9");
        assert_eq!(d.description, "HN: Comment by carol");
        assert_eq!(d.extended, "my reply");
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

        let fb = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v0/item/42.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": 42, "type": "story", "by": "alice",
                "title": "Cool", "url": "https://example.com/x"
            })))
            .mount(&fb)
            .await;
        Mock::given(method("GET"))
            .and(path("/v0/item/9.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": 9, "type": "comment", "by": "carol", "text": "hi"
            })))
            .mount(&fb)
            .await;

        let client = HnClient::with_base_urls(
            "psophis".into(),
            HackernewsConfig::default(),
            hn.uri(),
            fb.uri(),
        );
        let drafts = client.fetch().await.unwrap();
        assert_eq!(drafts.len(), 2);
        assert_eq!(drafts[0].url, "https://example.com/x");
        assert_eq!(drafts[1].url, "https://news.ycombinator.com/item?id=9");
        assert!(drafts[1].tags.contains(&"hackernews-comment".to_string()));
    }
}
