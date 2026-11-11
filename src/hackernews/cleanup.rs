//! `cleanup hackernews`: re-shape existing HN item bookmarks from fresh Algolia data
//! (stories rewrite to their article URL), and with `--link-discussions` add the HN
//! discussion link to article bookmarks tagged for it.

use std::collections::HashMap;

use log::debug;
use url::Url;

use super::{
    hn_link_line, AlgoliaClient, HackerNewsConfig, HackerNewsItem, HackerNewsItemId, HN_LINK,
};
use crate::backup::RawSink;
use crate::bookmark::{AccountState, Bookmark, BookmarkStore};
use crate::cleanup_pass::{run_pass, two_pass_verdict, CleanupPass, PassOutcome, Plan};
use crate::htmltext::collapse_whitespace;
use crate::keys::HackerNewsKeys;
use crate::source::SourceError;
use crate::timefmt::DateOpts;

/// The `cleanup hackernews` flags that aren't account settings. The default, which
/// `cleanup --all` runs, links no discussions.
#[derive(Default)]
pub struct HackerNewsCleanupOverrides {
    pub link_discussions: bool,
    /// `--tag-link`, in place of the account's `tag_link`.
    pub link_tag: Option<String>,
}

/// Options for `cleanup hackernews`.
pub struct HackerNewsCleanupOpts {
    /// Also link `link_tag`-tagged article bookmarks to their HN discussion.
    pub link_discussions: bool,
    pub dates: DateOpts,
    /// The account's tag vocabulary `sync` writes with, and the `link_tag` marker.
    pub config: HackerNewsConfig,
}

/// Re-shape every HN item bookmark from a fresh read (a story moves to its article),
/// keeping its tags, and its date unless `use_post_date` re-dates it. With
/// `link_discussions` set, [`link_discussions`] then links tagged articles.
pub async fn run<S: BookmarkStore + AccountState>(
    store: &S,
    client: &AlgoliaClient,
    opts: &HackerNewsCleanupOpts,
) -> Result<(), SourceError> {
    let hackernews_bookmarks =
        store.snapshot(|bookmark| HackerNewsItemId::try_from(&bookmark.url).is_ok());

    let ids: Vec<HackerNewsItemId> = hackernews_bookmarks
        .iter()
        .filter_map(|bookmark| HackerNewsItemId::try_from(&bookmark.url).ok())
        .collect();
    let items = client.fetch_items(&ids, &mut RawSink::disabled()).await?;

    let pass = HackerNewsCleanupPass {
        items,
        config: &opts.config,
    };
    let items = run_pass(store, &hackernews_bookmarks, "HN", opts.dates, &pass).await;

    // Each pass reaches its own verdict: this one plans from prefetched items, can't fail,
    // and must not vouch for the discussion lookups. Those read the view after this pass
    // wrote, so a merged bookmark is planned from the merged record.
    let links = if opts.link_discussions {
        Some(link_discussions(store, client, opts).await)
    } else {
        None
    };
    two_pass_verdict(
        ("items", items.into_result()),
        (
            "discussion links",
            links.map_or(Ok(()), PassOutcome::into_result),
        ),
    )
}

/// Link each `link_tag`-tagged article to its HN discussion, if it has one. Opt-in: one
/// Algolia query per tagged bookmark.
async fn link_discussions<S: BookmarkStore + AccountState>(
    store: &S,
    client: &AlgoliaClient,
    opts: &HackerNewsCleanupOpts,
) -> PassOutcome {
    // After the item pass: a story it merged onto a tagged article carries the tag, so this
    // pass picks it up and the account settles in one run.
    let candidates = store.snapshot(|bookmark| {
        HackerNewsItemId::try_from(&bookmark.url).is_err()
            && bookmark.tags.has(&opts.config.link_tag)
    });
    let pass = HackerNewsLinkPass {
        client,
        config: &opts.config,
    };
    run_pass(store, &candidates, "HN discussion", opts.dates, &pass).await
}

/// The item id in a note's `HN Link: <url>` marker. Sync points it at the bookmark's own
/// discussion, so it names the source item whatever the bookmark's URL.
fn hn_link_marker_id(note: &str) -> Option<HackerNewsItemId> {
    note.lines()
        .find_map(|line| line.trim().strip_prefix(HN_LINK))
        .and_then(|rest| Url::parse(rest.trim()).ok())
        .and_then(|url| HackerNewsItemId::try_from(&url).ok())
}

/// `note` with `hn_link` appended, unless it carries an `HN Link:` line already.
fn ensure_hn_link_note(note: &str, hn_link: &str) -> String {
    if note.lines().any(is_hn_link_line) {
        note.to_string()
    } else if note.is_empty() {
        hn_link.to_string()
    } else {
        format!("{note}\n\n{hn_link}")
    }
}

/// Whether `line` is an `HN Link:` line, indented or not.
fn is_hn_link_line(line: &str) -> bool {
    line.trim_start().starts_with(HN_LINK)
}

/// The `HN Link:` line of a note sync built, which never indents it.
fn generated_hn_link(note: &str) -> Option<&str> {
    note.lines().find(|line| line.starts_with(HN_LINK))
}

/// Re-shapes one HN item bookmark from its fresh item, keeping its tags and any stored note
/// verbatim, the `HN Link:` line appended when absent: an annotated story keeps its link.
struct HackerNewsCleanupPass<'a> {
    items: HashMap<HackerNewsItemId, HackerNewsItem>,
    config: &'a HackerNewsConfig,
}

impl CleanupPass for HackerNewsCleanupPass<'_> {
    async fn plan(&self, bookmark: &Bookmark) -> Result<Plan, SourceError> {
        let Ok(url_id) = HackerNewsItemId::try_from(&bookmark.url) else {
            return Ok(Plan::Skipped);
        };
        // A meta-post linking another item is stored at that item's URL, but its marker
        // names its own discussion: the URL's id would re-shape the wrong item.
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
        // Re-derive the bookmark from the fresh item (url/title/note/date), keeping the
        // stored bookmark's tags.
        let Some(mut new) = item
            .clone()
            .into_draft(self.config, &HackerNewsKeys::default())
            .map(|d| d.bookmark)
        else {
            return Ok(Plan::Skipped);
        };
        let mut tags = bookmark.tags.clone();
        tags.push_all(&new.tags);
        new.tags = tags;
        let generated_link = generated_hn_link(&new.note).map(str::to_string);
        if !bookmark.note.is_empty() {
            new.note = match generated_link {
                Some(link) => ensure_hn_link_note(&bookmark.note, &link),
                None => bookmark.note.clone(),
            };
        }
        Ok(Plan::Bookmark(new))
    }
}

/// Links one tagged article to its HN discussion: an `HN Link:` line, and the base tags for
/// the marker. In place, with no `timestamp`, so the driver keeps the stored date.
struct HackerNewsLinkPass<'a> {
    client: &'a AlgoliaClient,
    config: &'a HackerNewsConfig,
}

impl CleanupPass for HackerNewsLinkPass<'_> {
    async fn plan(&self, bookmark: &Bookmark) -> Result<Plan, SourceError> {
        // Unlike other early exits, an answer: the search ran and found no discussion, so
        // it counts as reached.
        let Some(id) = self.client.search_by_url(&bookmark.url).await? else {
            return Ok(Plan::Unchanged);
        };

        let hn_link = hn_link_line(Url::from(&id));
        let note = ensure_hn_link_note(&bookmark.note, &hn_link);

        let title = collapse_whitespace(&bookmark.title, " ");

        let mut tags = bookmark.tags.clone();
        tags.retain(|t| *t != self.config.link_tag);
        tags.push_all(&self.config.tags);

        Ok(Plan::Bookmark(Bookmark {
            url: bookmark.url.clone(),
            title,
            note,
            tags,
            timestamp: None,
            public: bookmark.public,
            read_later: bookmark.read_later,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{stored_at, tag_list};
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

    #[test]
    fn the_marker_id_is_read_from_a_whole_marker_line_only() {
        let id = |note: &str| hn_link_marker_id(note).map(|id| id.to_string());
        assert_eq!(
            id("my note\n\n  HN Link:   https://news.ycombinator.com/item?id=42  "),
            Some("42".to_string())
        );
        assert_eq!(
            id("see the HN Link: https://news.ycombinator.com/item?id=42"),
            None
        );
        assert_eq!(id("HN Link: https://example.com/elsewhere"), None);
        assert_eq!(id("HN Link: not a url"), None);
        assert_eq!(id("hn link: https://news.ycombinator.com/item?id=42"), None);
    }

    #[test]
    fn generated_hn_link_reads_only_an_unindented_line() {
        let line = "HN Link: https://news.ycombinator.com/item?id=42";
        assert_eq!(generated_hn_link(&format!("text\n\n{line}")), Some(line));
        assert_eq!(generated_hn_link(&format!("  {line}")), None);
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
            title: "old title".into(),
            note: "my own thoughts on this".into(),
            tags: tag_list(&["mine"]),
            ..stored_at("https://news.ycombinator.com/item?id=42")
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
        assert!(planned.tags.has("mine"), "{:?}", planned.tags);
        assert!(planned.tags.has("hackernews"), "{:?}", planned.tags);
    }

    #[tokio::test]
    async fn plan_decodes_the_fresh_algolia_title_once_not_the_stored_one() {
        let stored = Bookmark {
            title: "Tom &amp; Jerry".into(),
            ..stored_at("https://news.ycombinator.com/item?id=42")
        };
        let mut items = HashMap::new();
        items.insert(
            HackerNewsItemId::from(42u64),
            item(json!({
                "id": 42, "type": "story", "by": "alice",
                "title": "Tom &amp;amp; Jerry", "url": "https://example.com/x"
            })),
        );
        let config = HackerNewsConfig::default();
        let pass = HackerNewsCleanupPass {
            items,
            config: &config,
        };

        let planned = pass.plan(&stored).await.unwrap().bookmark().unwrap();
        assert_eq!(planned.title, stored.title);
    }

    #[tokio::test]
    async fn plan_does_not_duplicate_existing_hn_link_line() {
        let stored = Bookmark {
            title: "old title".into(),
            note: "my note\n\nHN Link: https://news.ycombinator.com/item?id=42".into(),
            tags: tag_list(&["mine"]),
            ..stored_at("https://news.ycombinator.com/item?id=42")
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
            title: "Meta post".into(),
            note: "HN Link: https://news.ycombinator.com/item?id=42".into(),
            tags: tag_list(&["hackernews"]),
            ..stored_at("https://news.ycombinator.com/item?id=999")
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
        assert_eq!(pass.plan(&stored).await.unwrap(), Plan::Skipped);
    }

    #[tokio::test]
    async fn plan_uses_generated_note_when_stored_note_empty() {
        let stored = Bookmark {
            title: "old title".into(),
            tags: tag_list(&["mine"]),
            ..stored_at("https://news.ycombinator.com/item?id=42")
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
