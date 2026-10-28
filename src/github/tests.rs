use super::*;
use crate::test_support::url;
use serde_json::json;

/// Header-level cases for [`rate_limit_message`], kept off the network: a 429 through
/// `RetryingClient::send` would spend the full retry budget (~12s) before the body is
/// ever read, which is too slow to pay on every `cargo test`.
fn headers(pairs: &[(&str, &str)]) -> reqwest::header::HeaderMap {
    let mut map = reqwest::header::HeaderMap::new();
    for (name, value) in pairs {
        map.insert(
            reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            reqwest::header::HeaderValue::from_str(value).unwrap(),
        );
    }
    map
}

#[test]
fn retry_after_as_a_date_is_not_rendered_as_seconds() {
    // RFC 7231 allows an HTTP-date, which must not be suffixed into "...GMTs".
    let message = rate_limit_message(
        reqwest::StatusCode::TOO_MANY_REQUESTS,
        &headers(&[("retry-after", "Sun, 30 Aug 2026 15:00:00 GMT")]),
        "",
    )
    .expect("retry-after marks a rate limit");
    assert!(!message.contains("GMTs"), "{message}");
    assert!(message.contains("Sun, 30 Aug 2026"), "{message}");
}

#[test]
fn retry_after_as_seconds_reads_as_a_duration() {
    let message = rate_limit_message(
        reqwest::StatusCode::FORBIDDEN,
        &headers(&[("retry-after", "60")]),
        "",
    )
    .expect("retry-after marks a rate limit");
    assert!(message.contains("60s"), "{message}");
}

#[test]
fn a_success_is_never_a_rate_limit() {
    // The status gate matters: a 200 carrying an exhausted budget is the *last*
    // request that succeeded, not a refusal.
    assert_eq!(
        rate_limit_message(
            reqwest::StatusCode::OK,
            &headers(&[("x-ratelimit-remaining", "0")]),
            "",
        ),
        None
    );
}

#[test]
fn an_unrelated_403_body_is_not_a_rate_limit() {
    assert_eq!(
        rate_limit_message(
            reqwest::StatusCode::FORBIDDEN,
            &headers(&[("x-ratelimit-remaining", "4999")]),
            r#"{"message":"Must have admin rights to Repository."}"#,
        ),
        None
    );
}

#[test]
fn a_blocked_or_sso_gated_repo_is_refused_for_good() {
    let forbidden = reqwest::StatusCode::FORBIDDEN;
    let tos_block = r#"{"message":"Repository access blocked","block":{"reason":"tos",
            "created_at":"2014-01-31T22:32:14Z","html_url":"https://github.com/tos"}}"#;
    assert!(permanent_refusal(forbidden, &headers(&[]), tos_block).is_some());
    let sso = headers(&[(
        "x-github-sso",
        "required; url=https://github.com/orgs/acme/sso?authorization_request=x",
    )]);
    assert!(permanent_refusal(forbidden, &sso, r#"{"message":"Resource protected"}"#).is_some());
    assert!(permanent_refusal(
        reqwest::StatusCode::UNAVAILABLE_FOR_LEGAL_REASONS,
        &headers(&[]),
        ""
    )
    .is_some());
}

#[test]
fn a_refusal_that_may_clear_is_not_permanent() {
    let forbidden = reqwest::StatusCode::FORBIDDEN;
    let denied = r#"{"message":"Must have admin rights to Repository."}"#;
    assert_eq!(permanent_refusal(forbidden, &headers(&[]), denied), None);
    assert_eq!(
        permanent_refusal(forbidden, &headers(&[]), "not json, block"),
        None
    );
    assert_eq!(
        permanent_refusal(forbidden, &headers(&[]), r#"{"block":"a string"}"#),
        None
    );
    // A list endpoint's partial results name the orgs left out; that is no refusal.
    let partial = headers(&[("x-github-sso", "partial-results; organizations=1,2")]);
    assert_eq!(permanent_refusal(forbidden, &partial, denied), None);
    // A rate limit stops the pass instead, even with a block-shaped body.
    let limited = headers(&[("x-ratelimit-remaining", "0")]);
    assert_eq!(
        permanent_refusal(forbidden, &limited, r#"{"block":{"reason":"tos"}}"#),
        None
    );
    let limited_behind_sso = headers(&[
        ("retry-after", "60"),
        (
            "x-github-sso",
            "required; url=https://github.com/orgs/acme/sso",
        ),
    ]);
    assert_eq!(permanent_refusal(forbidden, &limited_behind_sso, ""), None);
    let tos_block = r#"{"message":"Repository access blocked","block":{"reason":"tos"}}"#;
    assert_eq!(
        permanent_refusal(reqwest::StatusCode::BAD_GATEWAY, &headers(&[]), tos_block),
        None
    );
}

fn repo(value: serde_json::Value) -> GitHubRepo {
    serde_json::from_value(value).unwrap()
}

#[test]
fn draft_maps_fields_and_language_tag() {
    let d = repo(json!({
        "full_name": "owner/Repo",
        "html_url": "https://github.com/owner/Repo",
        "description": "A thing",
        "homepage": "https://example.com",
        "language": "Rust",
    }))
    .into_draft(&GitHubConfig::default())
    .unwrap();

    assert_eq!(d.bookmark.url.as_str(), "https://github.com/owner/Repo");
    assert_eq!(d.bookmark.title, "owner/Repo");
    assert_eq!(
        d.bookmark.note,
        "<blockquote>A thing</blockquote>\n\nProject homepage: https://example.com"
    );
    assert_eq!(
        d.bookmark.tags,
        vec!["github-star", "lang:rust", "author:github:owner"]
    );
    assert_eq!(d.dedup_key, "github.com/owner/repo");
}

#[test]
fn a_star_is_dated_in_utc_at_whole_seconds() {
    let starred = |starred_at: &str| -> GitHubStarredRepo {
        serde_json::from_value(json!({
            "starred_at": starred_at,
            "repo": { "full_name": "o/r", "html_url": "https://github.com/o/r" },
        }))
        .unwrap()
    };
    let date = |starred_at| {
        starred(starred_at)
            .into_draft(&GitHubConfig::default())
            .unwrap()
            .bookmark
            .timestamp
            .and_then(crate::timefmt::to_rfc3339)
    };

    assert_eq!(
        date("2023-01-02T04:04:05.75+01:00").as_deref(),
        Some("2023-01-02T03:04:05Z")
    );
    assert_eq!(date("not a date"), None);
}

#[test]
fn into_draft_skips_repo_with_unparseable_url() {
    // A repo whose `html_url` doesn't parse is dropped rather than producing a draft.
    let draft = repo(json!({ "full_name": "o/r", "html_url": "not a url" }))
        .into_draft(&GitHubConfig::default());
    assert!(draft.is_none(), "{draft:?}");
}

#[test]
fn github_note_wraps_description_and_keeps_homepage_outside() {
    assert_eq!(
        github_note(Some("A thing"), Some("https://h"), "https://github.com/o/r"),
        "<blockquote>A thing</blockquote>\n\nProject homepage: https://h"
    );
    // No description: fall back to the URL, no blockquote.
    assert_eq!(
        github_note(None, None, "https://github.com/o/r"),
        "https://github.com/o/r"
    );
    assert_eq!(
        github_note(Some(""), None, "https://github.com/o/r"),
        "https://github.com/o/r"
    );
}

#[test]
fn language_with_spaces_is_slugged() {
    // GitHub languages like "Jupyter Notebook" must not produce a space in the
    // tag (Pinboard would split it into two tags).
    let d = repo(json!({
        "full_name": "o/r", "html_url": "https://github.com/o/r",
        "language": "Jupyter Notebook"
    }))
    .into_draft(&GitHubConfig::default())
    .unwrap();
    assert_eq!(
        d.bookmark.tags,
        vec!["github-star", "lang:jupyter-notebook", "author:github:o"]
    );
}

#[test]
fn draft_without_description_or_language() {
    let d = repo(json!({
        "full_name": "o/r",
        "html_url": "https://github.com/o/r",
    }))
    .into_draft(&GitHubConfig::default())
    .unwrap();
    // Missing description falls back to the URL; no language -> no lang tag.
    assert_eq!(d.bookmark.note, "https://github.com/o/r");
    assert_eq!(d.bookmark.tags, vec!["github-star", "author:github:o"]);
}

#[test]
fn tags_list_replaces_the_default_base() {
    let cfg = GitHubConfig {
        tags: vec!["github-star".into(), "account:work".into()],
        ..GitHubConfig::default()
    };
    let d = repo(json!({ "full_name": "o/r", "html_url": "https://github.com/o/r" }))
        .into_draft(&cfg)
        .unwrap();
    assert_eq!(
        d.bookmark.tags,
        vec!["github-star", "account:work", "author:github:o"]
    );
}

#[test]
fn the_owner_tag_follows_its_prefix_and_an_empty_one_disables_it() {
    let draft = |author_prefix: &str| {
        let cfg = GitHubConfig {
            author_prefix: author_prefix.into(),
            ..GitHubConfig::default()
        };
        repo(json!({ "full_name": "Some-Org/r", "html_url": "https://github.com/Some-Org/r" }))
            .into_draft(&cfg)
            .unwrap()
            .bookmark
            .tags
    };
    assert_eq!(draft("owner:"), vec!["github-star", "owner:Some-Org"]);
    assert_eq!(draft(""), vec!["github-star"]);
}

#[test]
fn parse_link_next_extracts_next_page() {
    let link = "<https://api.github.com/user/starred?sort=created&page=2>; rel=\"next\", \
                <https://api.github.com/user/starred?sort=created&page=5>; rel=\"last\"";
    assert_eq!(parse_link_next(link), Some(2));
    let with_per_page = "<https://api.github.com/user/starred?sort=created&direction=desc\
                         &per_page=100&page=3>; rel=\"next\"";
    assert_eq!(parse_link_next(with_per_page), Some(3));
    // No next link (last page).
    assert_eq!(
        parse_link_next("<https://api.github.com/user/starred?page=1>; rel=\"prev\""),
        None
    );
}

#[test]
fn a_starred_page_is_caught_up_when_its_stars_are_on_pinboard() {
    use crate::test_support::stored_at;
    let star = |name: &str, html_url: &str| -> GitHubStarredRepo {
        serde_json::from_value(json!({ "starred_at": "2023-01-02T03:04:05Z",
            "repo": { "full_name": name, "html_url": html_url } }))
        .unwrap()
    };
    let repo = |name: &str| star(name, &format!("https://github.com/{name}"));
    let client =
        GitHubClient::with_base_url("tok".into(), GitHubConfig::default(), "unused".into());
    let synced_keys = client.synced_keys(&[
        stored_at("https://github.com/a/one"),
        stored_at("https://www.github.com/b/two"),
    ]);
    let caught_up = |page: Vec<GitHubStarredRepo>| {
        let page_len = page.len();
        let page_drafts: Vec<BookmarkDraft> = page
            .into_iter()
            .filter_map(|starred| starred.into_draft(&GitHubConfig::default()))
            .collect();
        page_is_caught_up(page_len, &page_drafts, &synced_keys)
    };

    assert!(
        caught_up(vec![repo("A/One"), repo("b/two")]),
        "stars already on Pinboard, in any case, with or without www."
    );
    assert!(!caught_up(vec![repo("a/one"), repo("c/new")]), "a new star");
    assert!(
        caught_up(vec![repo("a/one"), star("d/bad", "not a url")]),
        "a star that makes no draft adds nothing"
    );
}

#[test]
fn an_unsendable_token_names_its_setting_but_never_the_token() {
    let token = ConfiguredSecret {
        value: "SECRET\nTOKEN".into(),
        source: "the config's `token`",
    };
    let Err(err) = GitHubClient::new(token, GitHubConfig::default()) else {
        panic!("a token with a newline cannot be a header");
    };
    let message = format!("{err:#}");
    assert!(message.contains("the config's `token`"), "{message}");
    assert!(!message.contains("SECRET"), "{message}");
}

#[test]
fn dedup_key_only_matches_github() {
    let c =
        GitHubClient::with_base_url("t".into(), GitHubConfig::default(), GITHUB_API_BASE.into());
    assert_eq!(
        c.dedup_key(&url("https://github.com/o/r")).as_deref(),
        Some("github.com/o/r")
    );
    assert_eq!(c.dedup_key(&url("https://example.com/o/r")), None);
    // The www. alias folds onto the bare host so it dedups against a github.com
    // star with the same owner/repo.
    assert_eq!(
        c.dedup_key(&url("https://www.github.com/o/r")).as_deref(),
        Some("github.com/o/r")
    );
    // Other subdomains keep their own host in the key (they are not repo stars).
    assert_eq!(
        c.dedup_key(&url("https://gist.github.com/user/abcd1234"))
            .as_deref(),
        Some("gist.github.com/user/abcd1234")
    );
}
