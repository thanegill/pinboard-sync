use super::*;

#[test]
fn casing_preserves_multiword_camel() {
    assert_eq!(cased_subreddit("AskReddit"), "AskReddit");
    assert_eq!(cased_subreddit("ExplainLikeImFive"), "ExplainLikeImFive");
    assert_eq!(cased_subreddit("IAmA"), "IAmA");
}

#[test]
fn casing_lowercases_single_word_and_all_caps() {
    assert_eq!(cased_subreddit("rust"), "rust");
    assert_eq!(cased_subreddit("NEWS"), "news");
    assert_eq!(cased_subreddit("WTF"), "wtf");
    // Single-word title case is lowercased (only multi-word camel is kept).
    assert_eq!(cased_subreddit("Python"), "python");
    assert_eq!(cased_subreddit("Rust"), "rust");
}

fn item(kind: &str, fields: serde_json::Value) -> RedditSavedItem {
    let entry: RedditListingEntry =
        serde_json::from_value(serde_json::json!({ "kind": kind, "data": fields })).unwrap();
    entry.into_saved_item("old.reddit.com").unwrap()
}

#[test]
fn post_url_and_tags() {
    let it = item(
        "t3",
        serde_json::json!({
            "name": "t3_abc",
            "subreddit": "rust",
            "permalink": "/r/rust/comments/abc/title/",
            "title": "A title",
            "url": "https://example.com",
        }),
    );
    assert!(!it.is_comment);
    assert_eq!(
        it.bookmark_url("old.reddit.com"),
        "https://old.reddit.com/r/rust/comments/abc/title/"
    );
    assert_eq!(it.title, "A title");
    // A plain link post gets no `type:` tag (only image/video do).
    assert_eq!(
        it.tags(&RedditConfig::default()),
        vec!["reddit", "subreddit:rust"]
    );
}

#[test]
fn draft_titles_are_raw_text_with_whitespace_collapsed() {
    // `raw_json=1` already gives the literal title, so a `&amp;` in it is the poster's
    // own text, not an escape to undo.
    for (kind, fields) in [
        (
            "t3",
            serde_json::json!({ "title": "What &amp; means\n in <b>HTML</b>" }),
        ),
        (
            "t1",
            serde_json::json!({ "link_title": "What &amp; means\n in <b>HTML</b>" }),
        ),
    ] {
        let mut fields = fields;
        fields["name"] = "t3_abc".into();
        fields["subreddit"] = "rust".into();
        fields["permalink"] = "/r/rust/comments/abc/x/".into();
        let draft = item(kind, fields)
            .into_draft(&RedditConfig::default())
            .unwrap();
        assert_eq!(
            draft.bookmark.title, "What &amp; means in <b>HTML</b>",
            "{kind}"
        );
    }
}

#[test]
fn every_non_empty_base_tag_is_added() {
    let it = item(
        "t3",
        serde_json::json!({
            "name": "t3_abc", "subreddit": "rust",
            "permalink": "/r/rust/comments/abc/x/", "title": "T"
        }),
    );
    for (tags, want) in [
        (vec![], vec!["subreddit:rust"]),
        (vec![""], vec!["subreddit:rust"]),
        (vec!["", "reddit"], vec!["reddit", "subreddit:rust"]),
        (
            vec!["reddit", "saved"],
            vec!["reddit", "saved", "subreddit:rust"],
        ),
    ] {
        let cfg = RedditConfig {
            tags: tags.iter().map(|t| t.to_string()).collect(),
            ..RedditConfig::default()
        };
        assert_eq!(it.tags(&cfg), want, "tags = {tags:?}");
    }
}

#[test]
fn empty_subreddit_prefix_disables_the_subreddit_tag() {
    let it = item(
        "t3",
        serde_json::json!({
            "name": "t3_abc", "subreddit": "rust",
            "permalink": "/r/rust/comments/abc/x/", "title": "T"
        }),
    );
    let cfg = RedditConfig {
        subreddit_prefix: String::new(),
        ..RedditConfig::default()
    };
    assert_eq!(it.tags(&cfg), vec!["reddit"]);
}

#[test]
fn into_draft_skips_item_with_unparseable_url() {
    // A `domain` that can't form a valid URL (a space in the host) — the item is
    // dropped rather than producing a draft with no URL.
    let cfg = RedditConfig {
        domain: "bad host".into(),
        ..RedditConfig::default()
    };
    let it = item(
        "t3",
        serde_json::json!({
            "name": "t3_abc", "subreddit": "rust",
            "permalink": "/r/rust/comments/abc/x/", "title": "T"
        }),
    );
    assert!(it.into_draft(&cfg).is_none());
}

#[test]
fn post_tags_include_author_flair_and_type() {
    let it = item(
        "t3",
        serde_json::json!({
            "name": "t3_abc", "subreddit": "rust", "permalink": "/r/rust/comments/abc/x/",
            "title": "T", "author": "alice", "link_flair_text": "Help Wanted",
            "post_hint": "image"
        }),
    );
    let tags = it.tags(&RedditConfig::default());
    assert!(tags.contains(&"author:reddit:alice".to_string()));
    assert!(tags.contains(&"reddit-flair:help-wanted".to_string()));
    assert!(tags.contains(&"type:image".to_string()));
}

#[test]
fn flair_commas_are_slugged_like_whitespace() {
    // Pinboard tags may not contain commas any more than whitespace.
    for (flair, want) in [
        ("Help, Question", "reddit-flair:help-question"),
        ("a,b", "reddit-flair:a-b"),
        (",Meta,", "reddit-flair:meta"),
    ] {
        let it = item(
            "t3",
            serde_json::json!({
                "name": "t3_abc", "subreddit": "rust", "permalink": "/r/rust/comments/abc/x/",
                "title": "T", "link_flair_text": flair
            }),
        );
        assert_eq!(
            it.tags(&RedditConfig::default()),
            vec!["reddit", "subreddit:rust", want],
            "{flair:?}"
        );
    }
}

#[test]
fn tags_come_in_a_fixed_order() {
    let it = item(
        "t3",
        serde_json::json!({
            "name": "t3_abc", "subreddit": "rust", "permalink": "/r/rust/comments/abc/x/",
            "title": "T", "author": "alice", "link_flair_text": "Help Wanted",
            "post_hint": "image", "over_18": true
        }),
    );
    assert_eq!(
        it.tags(&RedditConfig::default()),
        vec![
            "reddit",
            "subreddit:rust",
            "nsfw",
            "author:reddit:alice",
            "reddit-flair:help-wanted",
            "type:image"
        ]
    );
}

#[test]
fn media_type_tag_is_gated_by_the_media_types_allowlist() {
    let img = item(
        "t3",
        serde_json::json!({
            "name": "t3_a", "subreddit": "rust", "permalink": "/r/rust/comments/a/x/",
            "title": "T", "post_hint": "image"
        }),
    );
    // Allowlisting only "video" drops the image tag; "" disables media tags.
    let only_video = RedditConfig {
        media_types: vec!["video".into()],
        ..RedditConfig::default()
    };
    assert!(!img.tags(&only_video).iter().any(|t| t.starts_with("type:")));
    assert!(img
        .tags(&RedditConfig::default())
        .contains(&"type:image".to_string()));

    // An empty media_prefix disables the tag even when the type is allowed.
    let no_prefix = RedditConfig {
        media_prefix: String::new(),
        ..RedditConfig::default()
    };
    assert!(!img.tags(&no_prefix).iter().any(|t| t.starts_with("type:")));
}

#[test]
fn text_and_link_posts_get_no_type_tag_and_comment_links_its_parent_thread() {
    // A self/text post gets no `type:` tag.
    let post = item(
        "t3",
        serde_json::json!({
            "name": "t3_a", "subreddit": "rust", "permalink": "/r/rust/comments/a/x/",
            "title": "T", "is_self": true, "selftext": "hi"
        }),
    );
    assert!(!post
        .tags(&RedditConfig::default())
        .iter()
        .any(|t| t.starts_with("type:")));

    let comment = item(
        "t1",
        serde_json::json!({
            "name": "t1_b", "subreddit": "rust", "permalink": "/r/rust/comments/a/x/b/",
            "link_title": "Parent", "link_permalink": "/r/rust/comments/a/x/", "body": "my reply"
        }),
    );
    assert_eq!(
        comment.note,
        "Thread: https://old.reddit.com/r/rust/comments/a/x/\n\n<blockquote>my reply</blockquote>"
    );
    // Comments get no `type:` tag.
    assert!(!comment
        .tags(&RedditConfig::default())
        .iter()
        .any(|t| t.starts_with("type:")));
}

#[test]
fn empty_self_post_has_no_duplicated_permalink_in_notes() {
    // A self-post with no body: Reddit's `url` is the post's own permalink, so
    // falling back to it would duplicate the bookmark URL in the notes.
    let post = item(
        "t3",
        serde_json::json!({
            "name": "t3_a", "subreddit": "rust", "permalink": "/r/rust/comments/a/x/",
            "title": "T", "is_self": true, "selftext": "",
            "url": "https://www.reddit.com/r/rust/comments/a/x/"
        }),
    );
    assert_eq!(post.note, "");
}

#[test]
fn empty_link_post_keeps_the_external_url_in_notes() {
    // A link post with no selftext keeps its external URL in the notes — that's
    // not the bookmark URL (which is the reddit permalink), so it's not a dup.
    let post = item(
        "t3",
        serde_json::json!({
            "name": "t3_a", "subreddit": "rust", "permalink": "/r/rust/comments/a/x/",
            "title": "T", "is_self": false, "selftext": "",
            "url": "https://example.com/article"
        }),
    );
    assert_eq!(post.note, "https://example.com/article");
}

#[test]
fn comment_gets_reddit_comment_tag_and_link_title() {
    let it = item(
        "t1",
        serde_json::json!({
            "name": "t1_xyz",
            "subreddit": "NEWS",
            "permalink": "/r/NEWS/comments/abc/title/xyz/",
            "link_title": "The story",
            "body": "my comment",
        }),
    );
    assert!(it.is_comment);
    assert_eq!(it.title, "The story");
    assert_eq!(it.note, "<blockquote>my comment</blockquote>");
    // All-caps subreddit lowercased.
    assert_eq!(
        it.tags(&RedditConfig::default()),
        vec!["reddit", "subreddit:news", "reddit-comment"]
    );
}

#[test]
fn notes_wrap_the_quoted_body_and_keep_links_outside() {
    let domain = "old.reddit.com";
    // Comment: thread link outside, body in a blockquote.
    assert_eq!(
        comment_note("hi", Some("/r/x/comments/a/t/"), domain),
        "Thread: https://old.reddit.com/r/x/comments/a/t/\n\n<blockquote>hi</blockquote>"
    );
    // Comment with no body: just the thread link, no empty blockquote.
    assert_eq!(
        comment_note("", Some("/r/x/comments/a/t/"), domain),
        "Thread: https://old.reddit.com/r/x/comments/a/t/"
    );
    // Comment with body but no resolvable thread: just the wrapped body.
    assert_eq!(
        comment_note("hi", None, domain),
        "<blockquote>hi</blockquote>"
    );
    // Self-post: selftext wrapped.
    assert_eq!(
        post_note("post body", "", true),
        "<blockquote>post body</blockquote>"
    );
    // Empty self-post: no notes (its url is its own permalink).
    assert_eq!(post_note("", "", true), "");
    // Link post: external URL is a bare fallback, not quoted.
    assert_eq!(
        post_note("", "https://example.com", false),
        "https://example.com"
    );
}

#[test]
fn over_18_adds_nsfw_tag() {
    let it = item(
        "t3",
        serde_json::json!({
            "name": "t3_abc",
            "subreddit": "gonewild",
            "permalink": "/r/gonewild/comments/abc/x/",
            "title": "x",
            "over_18": true,
        }),
    );
    assert!(it
        .tags(&RedditConfig::default())
        .contains(&"nsfw".to_string()));
}

#[test]
fn a_deleted_author_gets_no_author_tag() {
    for kind in ["t3", "t1"] {
        let it = item(
            kind,
            serde_json::json!({
                "name": format!("{kind}_abc"), "subreddit": "rust",
                "permalink": "/r/rust/comments/abc/x/def/", "title": "T",
                "author": "[deleted]"
            }),
        );
        assert_eq!(it.author, None, "{kind}");
        assert!(
            !it.tags(&RedditConfig::default())
                .iter()
                .any(|t| t.starts_with("author:")),
            "{kind}"
        );
    }
}

#[test]
fn malformed_child_is_skipped_not_failing_the_whole_page() {
    // The second child lacks the required `kind`, so it can't deserialize into a
    // RedditListingEntry. It must be dropped, keeping the good sibling rather than
    // failing the entire listing.
    let listing: RedditListing = serde_json::from_str(
        r#"{
                "kind": "Listing",
                "data": {
                    "after": null,
                    "children": [
                        { "kind": "t3", "data": { "name": "t3_a", "subreddit": "rust",
                          "permalink": "/r/rust/a/", "title": "A" } },
                        { "data": { "name": "t3_b" } }
                    ]
                }
            }"#,
    )
    .unwrap();
    assert_eq!(listing.data.children.len(), 1);
    assert_eq!(
        listing.data.children[0].fields.name.as_deref(),
        Some("t3_a")
    );
}

#[test]
fn all_children_failing_is_an_error_not_a_silent_empty_page() {
    // Both children lack the required `kind`, so a schema break drops every entry.
    // A non-empty page that parses to zero items must error rather than silently
    // yield an empty page (which would make sync exit 0 having imported nothing).
    let result: Result<RedditListing, _> = serde_json::from_str(
        r#"{
                "kind": "Listing",
                "data": {
                    "children": [
                        { "data": { "name": "t3_a" } },
                        { "data": { "name": "t3_b" } }
                    ]
                }
            }"#,
    );
    assert!(result.is_err());
}

#[test]
fn genuinely_empty_children_is_ok() {
    // An empty page is legitimate (nothing saved / end of listing): no error.
    let listing: RedditListing =
        serde_json::from_str(r#"{ "kind": "Listing", "data": { "after": null, "children": [] } }"#)
            .unwrap();
    assert!(listing.data.children.is_empty());
}

#[test]
fn non_array_children_still_fail() {
    // A body whose `children` isn't a JSON array (e.g. an anti-bot page shape) is
    // still a genuine error, not silently an empty page.
    let result: Result<RedditListing, _> =
        serde_json::from_str(r#"{ "kind": "Listing", "data": { "children": "nope" } }"#);
    assert!(result.is_err());
}

#[test]
fn non_post_non_comment_kinds_are_skipped() {
    let entry: RedditListingEntry = serde_json::from_value(serde_json::json!({
        "kind": "t5",
        "data": { "subreddit": "rust", "permalink": "/r/rust/" }
    }))
    .unwrap();
    assert!(entry.into_saved_item("old.reddit.com").is_none());
}

#[test]
fn a_kind_that_is_not_a_string_is_malformed() {
    for kind in [serde_json::json!({ "t3": null }), serde_json::json!(3)] {
        let entry = serde_json::from_value::<RedditListingEntry>(serde_json::json!({
            "kind": kind,
            "data": { "subreddit": "rust", "permalink": "/r/rust/comments/a/x/" }
        }));
        assert!(entry.is_err(), "accepted kind {kind}");
    }
}

#[test]
fn deserializes_a_listing_with_after_and_mixed_children() {
    let listing: RedditListing = serde_json::from_str(
        r#"{
                "kind": "Listing",
                "data": {
                    "after": "t3_next",
                    "children": [
                        { "kind": "t3", "data": { "name": "t3_a", "subreddit": "rust",
                          "permalink": "/r/rust/a/", "title": "A" } },
                        { "kind": "t1", "data": { "name": "t1_b", "subreddit": "rust",
                          "permalink": "/r/rust/a/b/", "link_title": "A", "body": "hi",
                          "over_18": true } }
                    ]
                }
            }"#,
    )
    .unwrap();
    assert_eq!(listing.data.after.as_deref(), Some("t3_next"));
    assert_eq!(listing.data.children.len(), 2);
    let items: Vec<_> = listing
        .data
        .children
        .into_iter()
        .filter_map(|e| e.into_saved_item("old.reddit.com"))
        .collect();
    assert_eq!(items.len(), 2);
    assert!(items[1].is_comment);
    assert!(items[1].over_18);
}
