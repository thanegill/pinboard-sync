//! Scraping the item IDs a user favorited from their `/favorites?id=<user>` pages and the
//! `&comments=t` stream of them.

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use html5ever::tendril::TendrilSink;
use html5ever::{ns, Attribute, LocalName, ParseOpts};
use log::{debug, warn};
use markup5ever_rcdom::{NodeData, RcDom};
use url::Url;

use super::{HackerNewsClient, HackerNewsItemId};
use crate::backup::{RawKind, RawSink};
use crate::http::{read_body, retry_after_up_to};
use crate::source::{page_is_caught_up, BookmarkDraft, DedupKey, Halt, SourceError};

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

/// One of a user's two favorites listings.
#[derive(Debug, Clone, Copy)]
pub(super) enum FavoritesStream {
    Stories,
    Comments,
}

impl FavoritesStream {
    /// The `backup` stream its pages are captured under, which also names it in messages.
    fn name(self) -> &'static str {
        match self {
            FavoritesStream::Stories => "favorites",
            FavoritesStream::Comments => "favorites-comments",
        }
    }

    /// What the `/favorites` URL appends to read this stream.
    fn query_suffix(self) -> &'static str {
        match self {
            FavoritesStream::Stories => "",
            FavoritesStream::Comments => "&comments=t",
        }
    }
}

/// What [`HackerNewsClient::collect_ids`] read from one stream.
pub(super) struct FavoriteIds {
    /// The favorited item IDs, newest first.
    pub ids: Vec<HackerNewsItemId>,
    /// The drafts the pages' items made, when they were looked up page by page.
    pub drafts: HashMap<HackerNewsItemId, BookmarkDraft>,
}

impl HackerNewsClient {
    /// Collect the item IDs of the `favorites` stream, following the "More" link across
    /// pages. With `synced_keys`, each page's items are shaped into drafts as it is read,
    /// and the walk stops at the first page with nothing new ([`is_caught_up`]).
    pub(super) async fn collect_ids(
        &self,
        favorites: FavoritesStream,
        synced_keys: Option<&HashSet<DedupKey>>,
        sink: &mut RawSink,
    ) -> Result<FavoriteIds, SourceError> {
        // The scraped page is the only record of a favorite HN's API never exposes, so
        // `backup` keeps it verbatim alongside the Algolia details.
        let stream = favorites.name();
        let mut out = Vec::new();
        let mut drafts = HashMap::new();
        let mut next = Some(format!(
            "{}/favorites?id={}{}",
            self.base,
            self.username,
            favorites.query_suffix()
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
            let body = self.favorites_page(&url, stream, page).await?;
            sink.push(stream, RawKind::Html, &body);
            let (scraped, more) = match parse_favorite_ids(&body) {
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
            let ids: Vec<HackerNewsItemId> = scraped
                .iter()
                .filter_map(|id| {
                    id.parse()
                        .map_err(|_| {
                            warn!(
                                "hn {stream} for '{}': skipping a row with the non-numeric \
                                 id {id:?}",
                                self.username
                            );
                        })
                        .ok()
                })
                .collect();
            if ids.is_empty() && !scraped.is_empty() {
                return Err(anyhow::anyhow!(
                    "hn {stream} for '{}' had no numeric item ids (page {page}); HN's markup \
                     may have changed",
                    self.username
                )
                .into());
            }
            let caught_up = match synced_keys {
                Some(synced_keys) => {
                    let page_drafts =
                        self.drafts_by_id(self.algolia.fetch_items(&ids, sink).await?);
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
        Ok(FavoriteIds { ids: out, drafts })
    }

    /// The body of the favorites page at `url`, page `page` of `stream`. A 429 halts the
    /// walk as [`Halt::RateLimited`], since every later page would draw one too.
    async fn favorites_page(
        &self,
        url: &str,
        stream: &str,
        page: usize,
    ) -> Result<String, SourceError> {
        let label = format!("hn {stream}");
        let resp = self
            .http
            .send_with(&label, retry_after_up_to(FAVORITES_MAX_RETRY_AFTER), || {
                self.http.get(url)
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
            return Err(SourceError::Halted(Halt::RateLimited(format!(
                "hn {stream} for '{}' returned {status}{retry_after}",
                self.username
            ))));
        }
        if !status.is_success() {
            return Err(
                anyhow::anyhow!("hn {stream} for '{}' returned {status}", self.username).into(),
            );
        }
        Ok(read_body(resp, true, || format!("reading {label} page {page}")).await?)
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
    synced_keys: &HashSet<DedupKey>,
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
    let elements = document_elements(html);
    let ids: Vec<String> = elements
        .iter()
        .filter(|e| e.is("tr") && e.has_class("athing"))
        .filter_map(|e| e.attr("id").map(str::to_owned))
        .collect();
    let has_marker = elements
        .iter()
        .any(|e| e.is("html") && e.attr("op") == Some("favorites"));
    if ids.is_empty() && !has_marker {
        return FavoritesPage::Unrecognized;
    }
    let more = elements
        .iter()
        .find(|e| e.is("a") && e.has_class("morelink"))
        .and_then(|e| e.attr("href"))
        .map(str::to_owned);
    FavoritesPage::Recognized { ids, more }
}

/// A parsed favorites-page element: its tag name, in any namespace, and its attributes.
struct Element {
    tag: LocalName,
    attrs: Vec<Attribute>,
}

impl Element {
    fn is(&self, tag: &str) -> bool {
        &*self.tag == tag
    }

    /// The value of the un-namespaced attribute `name`, its character references decoded.
    fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|attr| attr.name.ns == ns!() && &*attr.name.local == name)
            .map(|attr| &*attr.value)
    }

    /// Whether `class` is one of the whitespace-separated tokens of the `class`
    /// attribute, compared case-sensitively.
    fn has_class(&self, class: &str) -> bool {
        self.attr("class")
            .is_some_and(|classes| classes.split_whitespace().any(|token| token == class))
    }
}

/// Every element of `html` parsed as a whole document, in document order. A
/// `<template>`'s contents follow it, as though they were its children.
fn document_elements(html: &str) -> Vec<Element> {
    let dom = html5ever::parse_document(RcDom::default(), ParseOpts::default()).one(html);
    let mut elements = Vec::new();
    let mut pending = vec![dom.document.clone()];
    while let Some(node) = pending.pop() {
        if let NodeData::Element {
            name,
            attrs,
            template_contents,
            ..
        } = &node.data
        {
            pending.extend(template_contents.borrow().clone());
            elements.push(Element {
                tag: name.local.clone(),
                attrs: attrs.borrow().clone(),
            });
        }
        pending.extend(node.children.borrow().iter().rev().cloned());
    }
    elements
}
