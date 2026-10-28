//! The Algolia HN search API: item details by ID, and the story discussing a URL.

use std::collections::HashMap;
use std::time::Duration;

use anyhow::Context;
use serde::Deserialize;
use url::Url;

use super::{HackerNewsItem, HackerNewsItemId, HackerNewsItemKind};
use crate::backup::{RawKind, RawSink};
use crate::http::RetryingClient;
use crate::keys::url_key;
use crate::source::SourceError;

const ALGOLIA_BASE: &str = "https://hn.algolia.com";
/// Item IDs per Algolia `objectID:… OR …` query. The API rejects very long filter
/// strings (~200+ clauses), so stay well under that.
pub(super) const ITEM_BATCH: usize = 100;
/// The pause before each Algolia item chunk after the first.
pub(super) const ALGOLIA_CHUNK_PAUSE: Duration = Duration::from_millis(300);

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

/// The item a hit describes; fails when its `objectID` is not an item number.
impl TryFrom<AlgoliaHit> for HackerNewsItem {
    type Error = std::num::ParseIntError;
    fn try_from(h: AlgoliaHit) -> Result<Self, Self::Error> {
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
        Ok(HackerNewsItem {
            id: h.object_id.parse()?,
            kind,
            by: h.author.unwrap_or_default(),
            url: h.url,
            title: h.title,
            text: h.comment_text.or(h.story_text),
            created_at: h.created_at_i,
        })
    }
}

/// Reads HN items from the Algolia search API: all `cleanup hackernews` needs, and the
/// item details `sync` looks favorites up in.
pub struct AlgoliaClient {
    http: RetryingClient,
    /// API base (overridden in tests).
    base: String,
}

impl AlgoliaClient {
    pub fn new() -> anyhow::Result<Self> {
        Self::build(ALGOLIA_BASE.to_string())
    }

    pub(super) fn build(base: String) -> anyhow::Result<Self> {
        Ok(Self {
            http: super::retrying_client()?,
            base,
        })
    }

    /// Test constructor pointing the API at a mock server.
    #[cfg(test)]
    pub(crate) fn with_base_url(base: String) -> Self {
        Self::build(base).unwrap()
    }

    /// Batch-fetch item details by ID from the Algolia HN search API, keyed by ID.
    /// IDs are queried in chunks of [`ITEM_BATCH`] via `objectID:… OR …`; items that
    /// no longer exist are simply absent from the map.
    pub(super) async fn fetch_items(
        &self,
        ids: &[HackerNewsItemId],
        sink: &mut RawSink,
    ) -> Result<HashMap<HackerNewsItemId, HackerNewsItem>, SourceError> {
        let endpoint = format!("{}/api/v1/search", self.base);
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
            let items = search
                .hits
                .into_iter()
                .filter_map(|hit| HackerNewsItem::try_from(hit).ok());
            out.extend(items.map(|item| (HackerNewsItemId::from(item.id), item)));
        }
        Ok(out)
    }

    /// Find the HN story id discussing `url`, via the Algolia URL search. Returns
    /// the id only when a hit's URL matches `url` (so loose matches are ignored).
    pub(super) async fn search_by_url(
        &self,
        url: &Url,
    ) -> Result<Option<HackerNewsItemId>, SourceError> {
        let endpoint = format!("{}/api/v1/search", self.base);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(object_id: &str) -> AlgoliaHit {
        serde_json::from_value(serde_json::json!({ "objectID": object_id, "_tags": ["story"] }))
            .unwrap()
    }

    #[test]
    fn a_hit_is_an_item_only_when_its_object_id_is_a_number() {
        let item = HackerNewsItem::try_from(hit("42")).expect("a numeric objectID");
        assert_eq!(item.id, 42);
        assert!(HackerNewsItem::try_from(hit("story-42")).is_err());
    }
}
