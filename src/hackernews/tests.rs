use super::favorites::{is_caught_up, parse_favorite_ids, FavoritesPage};
use super::*;
use crate::test_support::url;
use serde_json::json;

fn item(value: serde_json::Value) -> HackerNewsItem {
    serde_json::from_value(value).unwrap()
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
    let synced_keys: HashSet<DedupKey> = [
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
    .into_draft(&HackerNewsConfig::default(), &HackerNewsKeys::default())
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
    // bookmark reads back through `HackerNewsClient::dedup_key` (`hn:<id>`) -- a
    // generic host+path key drops the `id` query and re-adds it every sync.
    let article = "https://news.ycombinator.com/item?id=999";
    let d = item(json!({
        "id": 42, "type": "story", "by": "alice",
        "title": "Meta post", "url": article
    }))
    .into_draft(&HackerNewsConfig::default(), &HackerNewsKeys::default())
    .unwrap();
    assert_eq!(d.bookmark.url.as_str(), article);
    assert_eq!(d.dedup_key, "hn:999");
    let client = HackerNewsClient::new("u".into(), HackerNewsConfig::default()).unwrap();
    assert_eq!(client.dedup_key(&d.bookmark.url), Some("hn:999".into()));
}

#[test]
fn text_post_without_url_bookmarks_the_hn_permalink() {
    let d = item(json!({
        "id": 7, "type": "story", "by": "bob",
        "title": "Ask HN: How?", "text": "<p>details</p>"
    }))
    .into_draft(&HackerNewsConfig::default(), &HackerNewsKeys::default())
    .unwrap();
    assert_eq!(
        d.bookmark.url.as_str(),
        "https://news.ycombinator.com/item?id=7"
    );
    assert_eq!(d.dedup_key, "hn:7");
    // The bookmark URL is already the HN permalink, so the notes carry only the post
    // text (no redundant `HN Link:`), with the inner HTML converted to Markdown.
    assert_eq!(d.bookmark.note, "<blockquote>details</blockquote>");
    // Ask HN: -> special-type tag.
    assert!(
        d.bookmark.tags.has("hackernews:ask-hn"),
        "{:?}",
        d.bookmark.tags
    );
}

#[test]
fn text_post_without_text_has_empty_notes() {
    let d = item(json!({
        "id": 8, "type": "story", "by": "bob", "title": "Ask HN: empty?"
    }))
    .into_draft(&HackerNewsConfig::default(), &HackerNewsKeys::default())
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
    .into_draft(&HackerNewsConfig::default(), &HackerNewsKeys::default())
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
fn an_algolia_title_is_entity_decoded_exactly_once() {
    let d = item(json!({
        "id": 12, "type": "story", "by": "dave",
        "title": "Tom &amp;amp; Jerry&#x27;s &lt;3", "url": "https://example.com/a"
    }))
    .into_draft(&HackerNewsConfig::default(), &HackerNewsKeys::default())
    .unwrap();
    assert_eq!(d.bookmark.title, "Tom &amp; Jerry's <3");
}

#[test]
fn comment_bookmarks_permalink_with_comment_tag() {
    let d = item(json!({
        "id": 9, "type": "comment", "by": "carol", "text": "my reply"
    }))
    .into_draft(&HackerNewsConfig::default(), &HackerNewsKeys::default())
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
    let draft = item(json!({ "id": 5, "type": "story", "by": "ann", "url": "not a url" }))
        .into_draft(&HackerNewsConfig::default(), &HackerNewsKeys::default());
    assert!(draft.is_none(), "{draft:?}");
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
    assert!(ids.is_empty(), "{ids:?}");
    assert_eq!(more, None);
}

#[test]
fn parse_favorite_ids_flags_markup_break() {
    // A recognizable HTML page whose favorites structure is gone: no rows and no
    // favorites marker means HN changed the markup.
    let html = r#"<html op="news"><body><div>Nothing here.</div></body></html>"#;
    assert_eq!(parse_favorite_ids(html), FavoritesPage::Unrecognized);
}

#[test]
fn parse_favorite_ids_reads_hn_shaped_markup() {
    let html = r#"<html lang="en" op="favorites"><head>
        <script>document.write("<table><tr class='athing' id='999'></tr></table>")</script>
        </head><body><table>
          <tr class='athing submission' id='4242'><td>a</td></tr>
          <tr class='spacer'><td></td></tr>
          <tr class=athing id=5151><td>b</td></tr>
          <tr class='athingish' id='777'><td>c</td></tr>
          <tr class='athing'><td>no id</td></tr>
          <tr><td><span class='athing' id='888'>not a row</span></td></tr>
          <tr class='comtr  athing' id='31337'><td>d</td></tr>
          <tr><td><a href='favorites?id=u&amp;p=3' class='morelink' rel='next'>More</a>
                  <a href='favorites?id=u&amp;p=4' class='morelink'>More</a></td></tr>
        </table></body></html>"#;
    assert_eq!(
        parse_favorite_ids(html),
        FavoritesPage::Recognized {
            ids: vec!["4242".into(), "5151".into(), "31337".into()],
            more: Some("favorites?id=u&p=3".into()),
        }
    );
}

#[test]
fn parse_favorite_ids_row_matching_edge_cases() {
    let html = "<table>
          <TR CLASS='submission athing' ID='1'><td></td></TR>
          <tr class='ATHING' id='2'><td></td></tr>
          <tr class='a\u{a0}athing' id='3'><td></td></tr>
          <template><tr class='athing' id='4'><td></td></tr></template>
          <tr class='athing' id=''><td></td></tr>
        </table>";
    let FavoritesPage::Recognized { ids, more } = parse_favorite_ids(html) else {
        panic!("expected a recognized favorites page");
    };
    assert_eq!(ids, vec!["1", "3", "4", ""]);
    assert_eq!(more, None);
}

#[test]
fn parse_favorite_ids_matches_svg_elements_but_not_xlink_href() {
    let svg = "<svg><tr class='athing' id='5'></tr>
          <a class='morelink' xlink:href='favorites?id=u&amp;p=9'>More</a></svg>
        <a class='morelink' href='favorites?id=u&amp;p=2'>More</a>";
    assert_eq!(
        parse_favorite_ids(svg),
        FavoritesPage::Recognized {
            ids: vec!["5".into()],
            more: None,
        }
    );
}

#[test]
fn parse_favorite_ids_marker_is_case_sensitive_and_may_come_late() {
    let marker_case = r#"<html op="Favorites"><body></body></html>"#;
    assert_eq!(parse_favorite_ids(marker_case), FavoritesPage::Unrecognized);
    let marker_late = r#"<p>x</p><html op="favorites"><body></body></html>"#;
    assert!(matches!(
        parse_favorite_ids(marker_late),
        FavoritesPage::Recognized { .. }
    ));
}

#[test]
fn parse_favorite_ids_reads_real_empty_favorites_pages() {
    for (stream, html) in [
        (
            "stories",
            include_str!("fixtures/favorites-stories-empty.html"),
        ),
        (
            "comments",
            include_str!("fixtures/favorites-comments-empty.html"),
        ),
    ] {
        assert_eq!(
            parse_favorite_ids(html),
            FavoritesPage::Recognized {
                ids: vec![],
                more: None,
            },
            "{stream}"
        );
        let unmarked = html.replace(r#"op="favorites""#, r#"op="news""#);
        assert_eq!(
            parse_favorite_ids(&unmarked),
            FavoritesPage::Unrecognized,
            "{stream} without the marker"
        );
    }
}

#[test]
fn hn_item_id_extracts_only_from_hn_item_urls() {
    let id = HackerNewsItemId::try_from(&url("https://news.ycombinator.com/item?id=42")).unwrap();
    assert_eq!(id.to_string(), "42");
    // And round-trips back to the discussion permalink.
    assert_eq!(
        Url::from(&id).as_str(),
        "https://news.ycombinator.com/item?id=42"
    );
    HackerNewsItemId::try_from(&url("https://example.com/item?id=42")).unwrap_err();
    HackerNewsItemId::try_from(&url("https://news.ycombinator.com/news")).unwrap_err();
}

#[test]
fn hn_item_id_parses_only_digits() {
    let parse = |text: &str| text.parse::<HackerNewsItemId>().map(|id| id.to_string());
    assert_eq!(parse("42").unwrap(), "42");
    assert_eq!(
        parse("007").unwrap(),
        "7",
        "the same id as `From<u64>` makes"
    );
    for refused in [
        "",
        "+7",
        "-7",
        " 7",
        "7a",
        "99<script>",
        "1 OR author:x",
        "1e3",
    ] {
        parse(refused).expect_err(refused);
    }
    parse("99999999999999999999999").expect_err("past u64");
}

#[test]
fn dedup_key_distinguishes_hn_items_from_articles() {
    let c = HackerNewsClient::new("u".into(), HackerNewsConfig::default()).unwrap();
    assert_eq!(
        c.dedup_key(&url("https://news.ycombinator.com/item?id=42")),
        Some("hn:42".into())
    );
    assert_eq!(
        c.dedup_key(&url("https://example.com/x")),
        Some("example.com/x".into())
    );
}

/// Syncs a story linking `article`, runs `cleanup reddit` onto `domain`, and asserts the
/// bookmark moved to `cleaned_url` and that syncing the story again writes nothing.
async fn assert_cleanup_reddit_converges(domain: &str, article: &str, cleaned_url: &str) {
    use crate::reddit::cleanup::{run, RedditCleanupOpts};
    use crate::reddit::model::RedditConfig;
    use crate::test_support::{replay, FakePinboard, FakeReddit, NO_DATING};

    let client = HackerNewsClient::new("u".into(), HackerNewsConfig::default())
        .unwrap()
        .with_reddit_domains(vec![domain.into()]);
    let drafts = [client.story_draft(article)];
    let synced = client.sync_once(&drafts, &[]).await;
    assert_eq!(synced.len(), 1);

    let pinboard = FakePinboard {
        all: synced.clone(),
        ..Default::default()
    };
    let config = RedditConfig {
        domain: domain.into(),
        ..Default::default()
    };
    let opts = RedditCleanupOpts {
        mark_nsfw: false,
        fix_titles: false,
        config: config.clone(),
        dates: NO_DATING,
        configured: vec![config],
    };
    run(&pinboard.cleanup_store(), None::<&FakeReddit>, &opts)
        .await
        .unwrap();
    let cleaned = replay(&synced, &pinboard.ops.borrow());
    assert_eq!(cleaned[0].url.as_str(), cleaned_url);

    assert_eq!(client.sync_once(&drafts, &cleaned).await, cleaned);
}

#[tokio::test]
async fn a_story_linking_reddit_is_not_re_added_after_cleanup_reddit_moves_it() {
    assert_cleanup_reddit_converges(
        "old.reddit.com",
        "https://www.reddit.com/r/rust/comments/abc/title/",
        "https://old.reddit.com/r/rust/comments/abc/title/",
    )
    .await;
}

#[tokio::test]
async fn a_story_linking_reddit_is_not_re_added_after_cleanup_reddit_moves_it_to_a_mirror() {
    assert_cleanup_reddit_converges(
        "reddit.mirror.example",
        "https://www.reddit.com/r/rust/comments/abc/title/",
        "https://reddit.mirror.example/r/rust/comments/abc/title/",
    )
    .await;
}

#[tokio::test]
async fn a_story_linking_a_reddit_over18_interstitial_is_not_re_added_after_cleanup_reddit() {
    assert_cleanup_reddit_converges(
        "old.reddit.com",
        "https://www.reddit.com/over18?dest=https%3A%2F%2Fwww.reddit.com%2Fr%2Fx%2Fcomments%2Fa%2Ft%2F",
        "https://old.reddit.com/r/x/comments/a/t/",
    )
    .await;
    assert_cleanup_reddit_converges(
        "old.reddit.com",
        "https://www.reddit.com/over18?dest=https%3A%2F%2Fexample.com%2Fpost",
        "https://example.com/post",
    )
    .await;
    assert_cleanup_reddit_converges(
        "reddit.mirror.example",
        "https://www.reddit.com/over18?dest=https%3A%2F%2Fwww.reddit.com%2Fover18%3Fdest%3Dhttps%253A%252F%252Fwww.reddit.com%252Fr%252Fx%252Fcomments%252Fa%252Ft%252F",
        "https://reddit.mirror.example/r/x/comments/a/t/",
    )
    .await;
}

#[tokio::test]
async fn a_story_linking_the_bare_reddit_root_is_not_re_added_after_cleanup_reddit() {
    assert_cleanup_reddit_converges(
        "old.reddit.com",
        "https://www.reddit.com/",
        "https://old.reddit.com/",
    )
    .await;
    assert_cleanup_reddit_converges(
        "reddit.mirror.example",
        "https://www.reddit.com/",
        "https://reddit.mirror.example/",
    )
    .await;
}
