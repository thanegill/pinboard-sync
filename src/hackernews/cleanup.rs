//! `cleanup hackernews`: re-shape existing HN item bookmarks from fresh Algolia data
//! (stories rewrite to their article URL), and with `--link-discussions` add the HN
//! discussion link to article bookmarks tagged for it.

use std::collections::HashMap;

use anyhow::anyhow;
use log::debug;
use url::Url;

use super::{
    HackerNewsClient, HackerNewsConfig, HackerNewsItem, HackerNewsItemId, ALGOLIA_BASE, HN_BASE,
};
use crate::backup::RawSink;
use crate::bookmark::{AccountState, Bookmark, BookmarkStore};
use crate::cleanup_pass::{run_pass, CleanupPass, PassOutcome, Plan};
use crate::htmltext::html_to_plain;
use crate::source::{extend_unique, SourceError};
use crate::timefmt::DateOpts;

/// Options for `cleanup hackernews`.
pub struct HackerNewsCleanupOpts {
    /// Also link `link_tag`-tagged article bookmarks to their HN discussion.
    pub link_discussions: bool,
    /// Whether and how to re-date bookmarks by the source item's creation time.
    pub dates: DateOpts,
}

impl HackerNewsClient {
    /// Client for `cleanup hackernews`: only the Algolia API is used (no favorites
    /// scraping), so no username is needed.
    pub fn for_cleanup(config: HackerNewsConfig) -> anyhow::Result<Self> {
        Self::build(
            String::new(),
            config,
            HN_BASE.to_string(),
            ALGOLIA_BASE.to_string(),
        )
    }
}

/// Normalize existing `news.ycombinator.com/item?id=*` bookmarks: re-fetch each item and
/// re-shape it (stories rewrite to the article URL, deleting the old HN URL;
/// comments/text posts update in place), preserving existing tags and the creation time.
pub async fn run<S: BookmarkStore + AccountState>(
    store: &S,
    client: &HackerNewsClient,
    opts: &HackerNewsCleanupOpts,
) -> Result<(), SourceError> {
    // Taken from the run's live view, so an earlier source's writes are already
    // reflected.
    let hackernews_bookmarks: Vec<Bookmark> = store
        .snapshot()
        .into_iter()
        .filter(|bookmark| HackerNewsItemId::try_from(&bookmark.url).is_ok())
        .collect();

    // Batch-fetch every referenced item once.
    let ids: Vec<HackerNewsItemId> = hackernews_bookmarks
        .iter()
        .filter_map(|bookmark| HackerNewsItemId::try_from(&bookmark.url).ok())
        .collect();
    let items = client.fetch_items(&ids, &mut RawSink::disabled()).await?;

    let pass = HackerNewsCleanupPass {
        items,
        config: &client.config,
    };
    let items = run_pass(store, &hackernews_bookmarks, "HN", opts.dates, &pass).await;

    // Each pass reaches its own verdict. Merging the tallies first would let this
    // pass's bookmarks — which are planned from prefetched items and so can never
    // fail — vouch for the discussion lookups, which are the only per-item network
    // calls here and can fail on their own.
    // Its candidates come from the view *after* the item pass wrote, so a bookmark that
    // pass merged is re-planned from the merged record rather than from a stale copy.
    let links = match opts.link_discussions {
        true => Some(link_discussions(store, client, opts).await),
        false => None,
    };
    // Both passes ran, so report both failures: letting the first `?` short-circuit
    // would hide a wholesale failure of the second behind the first's message.
    match (
        items.into_result(),
        links.map_or(Ok(()), PassOutcome::into_result),
    ) {
        // Flattening the pair loses each one's variant, which costs nothing here:
        // HackerNews is public, so neither pass can produce a re-auth to act on.
        (Err(items_err), Err(links_err)) => Err(SourceError::Other(anyhow!(
            "{items_err:#}; and for discussion links: {links_err:#}"
        ))),
        (Err(e), Ok(())) | (Ok(()), Err(e)) => Err(e),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// For each article bookmark tagged `link_tag`, look it up on HN by URL and, if it has a
/// discussion, add `HN Link: <discussion>` to the notes and swap the marker tag for the
/// base HN tags (update in place). Default-off (opt-in via `--link-discussions`) because
/// it issues one Algolia query per tagged bookmark.
async fn link_discussions<S: BookmarkStore + AccountState>(
    store: &S,
    client: &HackerNewsClient,
    opts: &HackerNewsCleanupOpts,
) -> PassOutcome {
    // Read after the item pass, so a bookmark it merged into is seen as it now stands.
    // A story merged onto a `link_tag`-tagged article carries that tag into the merged
    // record, so this pass picks it up in the same run and the account settles in one.
    let candidates: Vec<Bookmark> = store
        .snapshot()
        .into_iter()
        .filter(|bookmark| {
            HackerNewsItemId::try_from(&bookmark.url).is_err()
                && bookmark.tags.contains(&client.config.link_tag)
        })
        .collect();
    run_pass(
        store,
        &candidates,
        "HN discussion",
        opts.dates,
        &HackerNewsLinkPass { client },
    )
    .await
}

/// The HN item id referenced by a note's `HN Link: <url>` back-link marker, if the note
/// carries one whose URL is an HN item permalink. Sync writes this marker pointing at the
/// bookmark's own discussion, so it identifies the true source item independently of the
/// bookmark's URL.
fn hn_link_marker_id(note: &str) -> Option<HackerNewsItemId> {
    note.lines()
        .find_map(|line| line.trim().strip_prefix("HN Link:"))
        .and_then(|rest| Url::parse(rest.trim()).ok())
        .and_then(|url| HackerNewsItemId::try_from(&url).ok())
}

/// Appends the `HN Link: <url>` back-link line to a note, skipping when the note already
/// carries one. An empty note becomes the bare link.
fn ensure_hn_link_note(note: &str, hn_link: &str) -> String {
    if note
        .lines()
        .any(|line| line.trim_start().starts_with("HN Link:"))
    {
        note.to_string()
    } else if note.is_empty() {
        hn_link.to_string()
    } else {
        format!("{note}\n\n{hn_link}")
    }
}

/// Re-shapes one favorited HN item bookmark: re-fetch via Algolia and re-derive the
/// draft (stories rewrite to the article URL; comments/text posts update in place),
/// preserving existing tags, privacy, and any note the user added by hand. A stored
/// note is kept verbatim, but the regenerated `HN Link:` back-link is appended to it
/// when absent so a hand-annotated story doesn't lose its discussion link.
struct HackerNewsCleanupPass<'a> {
    items: HashMap<HackerNewsItemId, HackerNewsItem>,
    config: &'a HackerNewsConfig,
}

impl CleanupPass for HackerNewsCleanupPass<'_> {
    async fn plan(&self, bookmark: &Bookmark) -> Result<Plan, SourceError> {
        // The pass is filtered to HN item URLs, so this matches.
        let Ok(url_id) = HackerNewsItemId::try_from(&bookmark.url) else {
            return Ok(Plan::Skipped);
        };
        // A meta-post whose submitted article URL is itself an HN item permalink is
        // stored AT that linked item's URL, but its note's `HN Link:` marker points at
        // the meta-post's own discussion (a different id). Reading the URL as the item id
        // would fetch and re-shape the *wrong* item (rewriting title/tags, and the URL if
        // it's an external article), so leave such a bookmark untouched.
        if let Some(marker_id) = hn_link_marker_id(&bookmark.note) {
            if marker_id != url_id {
                debug!(
                    "cleanup HN: leaving {} unchanged; its note's HN Link marker \
                     references item {marker_id}, not the URL's item {url_id} (meta-post)",
                    bookmark.url
                );
                return Ok(Plan::Skipped);
            }
        }
        let Some(item) = self.items.get(&url_id) else {
            return Ok(Plan::Skipped);
        };
        // Re-derive the bookmark from the fresh item (url/title/note/date), then preserve
        // the stored bookmark's existing tags and privacy on the re-write.
        let Some(mut new) = item.clone().into_draft(self.config).map(|d| d.bookmark) else {
            return Ok(Plan::Skipped);
        };
        let mut tags = bookmark.tags.clone();
        extend_unique(&mut tags, &new.tags);
        new.tags = tags;
        new.public = bookmark.public;
        new.read_later = bookmark.read_later;
        // Keep the user's stored note, but make sure the regenerated `HN Link:`
        // back-link survives the re-shape (append it once, never duplicated).
        let generated_link = new
            .note
            .lines()
            .find(|line| line.starts_with("HN Link:"))
            .map(str::to_string);
        if !bookmark.note.is_empty() {
            new.note = match generated_link {
                Some(link) => ensure_hn_link_note(&bookmark.note, &link),
                None => bookmark.note.clone(),
            };
        }
        Ok(Plan::Bookmark(new))
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
    async fn plan(&self, bookmark: &Bookmark) -> Result<Plan, SourceError> {
        // Unlike the other passes' early exits, this one is an *answer* from Algolia —
        // the search ran and the article simply has no discussion — so it counts as
        // having reached the source.
        let Some(id) = self.client.search_by_url(&bookmark.url).await? else {
            return Ok(Plan::Unchanged);
        };

        let hn_link = format!("HN Link: {}", Url::from(&id));
        let note = ensure_hn_link_note(&bookmark.note, &hn_link);

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

        Ok(Plan::Bookmark(Bookmark {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::url;
    use serde_json::json;

    fn item(value: serde_json::Value) -> HackerNewsItem {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn ensure_hn_link_note_appends_duplicates_and_handles_empty() {
        let link = "HN Link: https://news.ycombinator.com/item?id=42";
        assert_eq!(ensure_hn_link_note("", link), link);
        assert_eq!(
            ensure_hn_link_note("my note", link),
            format!("my note\n\n{link}")
        );
        let already = format!("my note\n\n{link}");
        assert_eq!(ensure_hn_link_note(&already, link), already);
    }

    #[test]
    fn ensure_hn_link_note_matches_only_a_marker_line_not_mid_prose() {
        let link = "HN Link: https://news.ycombinator.com/item?id=42";

        // "HN Link:" appearing mid-prose is not a marker line, so the back-link is
        // appended rather than the note being treated as already-linked.
        let prose = "better than the HN Link: thread I saw earlier";
        assert_eq!(
            ensure_hn_link_note(prose, link),
            format!("{prose}\n\n{link}")
        );

        // A real marker line (even with leading whitespace) is left unchanged.
        let with_marker = format!("my note\n\n  {link}");
        assert_eq!(ensure_hn_link_note(&with_marker, link), with_marker);
    }

    fn story_pass_items() -> HashMap<HackerNewsItemId, HackerNewsItem> {
        let mut items = HashMap::new();
        items.insert(
            HackerNewsItemId::from(42u64),
            item(json!({
                "id": 42, "type": "story", "by": "alice",
                "title": "Cool thing", "url": "https://example.com/x"
            })),
        );
        items
    }

    #[tokio::test]
    async fn plan_appends_hn_link_to_user_note_while_merging_base_tags() {
        let stored = Bookmark {
            url: url("https://news.ycombinator.com/item?id=42"),
            title: "old title".into(),
            note: "my own thoughts on this".into(),
            tags: vec!["mine".into()],
            timestamp: None,
            public: false,
            read_later: false,
        };
        let items = story_pass_items();
        let config = HackerNewsConfig::default();
        let pass = HackerNewsCleanupPass {
            items,
            config: &config,
        };

        let planned = pass.plan(&stored).await.unwrap().bookmark().unwrap();
        // The user's note survives the re-shape, and the regenerated `HN Link:`
        // back-link is appended rather than clobbering it.
        assert_eq!(
            planned.note,
            "my own thoughts on this\n\nHN Link: https://news.ycombinator.com/item?id=42"
        );
        // The re-derived url/title still apply and the base tags still merge in.
        assert_eq!(planned.url.as_str(), "https://example.com/x");
        assert_eq!(planned.title, "Cool thing");
        assert!(planned.tags.contains(&"mine".to_string()));
        assert!(planned.tags.contains(&"hackernews".to_string()));
    }

    #[tokio::test]
    async fn plan_does_not_duplicate_existing_hn_link_line() {
        let stored = Bookmark {
            url: url("https://news.ycombinator.com/item?id=42"),
            title: "old title".into(),
            note: "my note\n\nHN Link: https://news.ycombinator.com/item?id=42".into(),
            tags: vec!["mine".into()],
            timestamp: None,
            public: false,
            read_later: false,
        };
        let items = story_pass_items();
        let config = HackerNewsConfig::default();
        let pass = HackerNewsCleanupPass {
            items,
            config: &config,
        };

        let planned = pass.plan(&stored).await.unwrap().bookmark().unwrap();
        // A note that already carries the back-link is left untouched.
        assert_eq!(
            planned.note,
            "my note\n\nHN Link: https://news.ycombinator.com/item?id=42"
        );
    }

    #[tokio::test]
    async fn plan_skips_meta_post_whose_url_is_a_different_hn_item() {
        // A favorited meta-post: item 42 whose submitted article URL is itself an HN
        // item permalink (item 999). Sync stored it AT item?id=999 with item 42's note
        // carrying the `HN Link:` marker to item 42. Reading the URL as item 999 and
        // re-shaping it would corrupt the bookmark as the wrong item, so cleanup must
        // leave it unchanged rather than rewrite its url/title/tags.
        let stored = Bookmark {
            url: url("https://news.ycombinator.com/item?id=999"),
            title: "Meta post".into(),
            note: "HN Link: https://news.ycombinator.com/item?id=42".into(),
            tags: vec!["hackernews".into()],
            timestamp: None,
            public: false,
            read_later: false,
        };
        // The fetched item 999 is a *different* item (an external article) that a
        // re-shape would rewrite the bookmark to.
        let mut items = HashMap::new();
        items.insert(
            HackerNewsItemId::from(999u64),
            item(json!({
                "id": 999, "type": "story", "by": "somebody",
                "title": "Unrelated article", "url": "https://elsewhere.example/a"
            })),
        );
        let config = HackerNewsConfig::default();
        let pass = HackerNewsCleanupPass {
            items,
            config: &config,
        };
        // Skipped, not Unchanged: the meta-post is filtered out locally, so it is no
        // evidence that Algolia answered.
        assert!(matches!(pass.plan(&stored).await.unwrap(), Plan::Skipped));
    }

    #[tokio::test]
    async fn plan_uses_generated_note_when_stored_note_empty() {
        let stored = Bookmark {
            url: url("https://news.ycombinator.com/item?id=42"),
            title: "old title".into(),
            note: String::new(),
            tags: vec!["mine".into()],
            timestamp: None,
            public: false,
            read_later: false,
        };
        let items = story_pass_items();
        let config = HackerNewsConfig::default();
        let pass = HackerNewsCleanupPass {
            items,
            config: &config,
        };

        let planned = pass.plan(&stored).await.unwrap().bookmark().unwrap();
        // With no stored note, the reconstructed `HN Link:` note is used as-is.
        assert_eq!(
            planned.note,
            "HN Link: https://news.ycombinator.com/item?id=42"
        );
    }
}

#[cfg(test)]
mod net_tests;
