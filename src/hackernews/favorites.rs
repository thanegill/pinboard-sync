//! Scraping the item IDs a user favorited from their `/favorites?id=<user>` pages and the
//! `&comments=t` stream of them.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Context;
use log::{debug, warn};
use scraper::{Html, Selector};
use url::Url;

use super::{HackerNewsClient, HackerNewsItemId};
use crate::backup::{RawKind, RawSink};
use crate::http::retry_after_up_to;
use crate::source::{page_is_caught_up, BookmarkDraft, SourceError};

/// The longest `Retry-After` a favorites-page 429 is waited out for, once. A 429 asking
/// for longer, or for nothing, fails the fetch at once rather than holding the run open.
const FAVORITES_MAX_RETRY_AFTER: Duration = Duration::from_secs(120);
/// The pause before each favorites page after a fetch's first, the comments stream's
/// first page included.
pub(super) const FAVORITES_PAGE_PAUSE: Duration = Duration::from_millis(500);
/// Cap on favorites pages followed via the "More" link (30 items per page, so this
/// covers thousands of favorites). Bounds a malformed or looping "More" link so the
/// scrape can't spin forever.
pub(super) const MAX_FAVORITES_PAGES: usize = 100;

impl HackerNewsClient {
    /// Collect favorited item IDs from the stories page (`comments=false`) or the
    /// comments page (`comments=true`), following the "More" link across pages. With
    /// `synced_keys`, each page's items are looked up as it is read and shaped into
    /// `drafts`, and the walk stops at the first page that has nothing new
    /// ([`is_caught_up`]).
    pub(super) async fn collect_ids(
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
            if !status.is_success() {
                return Err(anyhow::anyhow!(
                    "hn {stream} for '{}' returned {status}",
                    self.username
                )
                .into());
            }
            let body = resp
                .text()
                .await
                .map_err(reqwest::Error::without_url)
                .with_context(|| format!("reading {label} page {page}"))?;
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
    pub(super) fn resolve(&self, href: &str, stream: &str) -> Option<String> {
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
}

/// Whether a favorites page would give `sync` nothing to write: every item on it that
/// has a draft in `drafts` has its `dedup_key` among `synced_keys`. Favorites list newest
/// first, so the pages after it hold nothing new either. A page whose items make no draft
/// at all (gone from Algolia, or unparseable) is caught up too; an empty page proves
/// nothing.
pub(super) fn is_caught_up(
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

/// Outcome of parsing a favorites page. `Unrecognized` means the page had neither
/// favorite rows nor the favorites-page marker: HN's markup likely changed, so the
/// scrape should fail loudly rather than silently import nothing.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum FavoritesPage {
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
pub(super) fn parse_favorite_ids(html: &str) -> FavoritesPage {
    static ROW: LazyLock<Selector> = LazyLock::new(|| selector("tr.athing"));
    static MARKER: LazyLock<Selector> = LazyLock::new(|| selector(r#"html[op="favorites"]"#));
    static MORE: LazyLock<Selector> = LazyLock::new(|| selector("a.morelink"));

    let doc = Html::parse_document(html);
    let ids: Vec<String> = doc
        .select(&ROW)
        .filter_map(|e| e.value().attr("id").map(str::to_string))
        .collect();
    if ids.is_empty() && doc.select(&MARKER).next().is_none() {
        return FavoritesPage::Unrecognized;
    }
    let next = doc
        .select(&MORE)
        .next()
        .and_then(|e| e.value().attr("href").map(str::to_string));
    FavoritesPage::Recognized { ids, more: next }
}

fn selector(css: &str) -> Selector {
    Selector::parse(css).unwrap_or_else(|e| panic!("invalid selector {css:?}: {e}"))
}
