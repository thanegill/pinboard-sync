use super::*;
use crate::test_support::url;
use serde_json::json;

const ID: &str = "aa5a315d61ae9438b18d";

fn gist(value: serde_json::Value) -> GitHubGist {
    serde_json::from_value(value).unwrap()
}

/// A gist by `someone` with two files, as `/gists/starred` lists it.
fn two_file_gist(description: Option<&str>) -> GitHubGist {
    gist(json!({
        "id": ID,
        "html_url": format!("https://gist.github.com/{ID}"),
        "description": description,
        "files": {
            "b.rs": { "filename": "b.rs", "language": "Rust" },
            "a.py": { "filename": "a.py", "language": "Python" },
            "c.rs": { "filename": "c.rs", "language": "Rust" },
            "notes": { "filename": "notes", "language": null },
        },
        "owner": { "login": "Someone" },
        "created_at": "2020-01-02T03:04:05Z",
        "updated_at": "2024-05-06T07:08:09Z",
    }))
}

fn default_config() -> GistConfig {
    crate::github::GitHubConfig::default().gists
}

#[test]
fn a_gist_draft_is_shaped_like_a_star() {
    let draft = two_file_gist(Some("A  handy\nscript"))
        .into_draft(&default_config())
        .unwrap();
    let bookmark = draft.bookmark;
    assert_eq!(
        bookmark.url.as_str(),
        format!("https://gist.github.com/Someone/{ID}"),
        "the owner form cleanup canonicalizes to, not the API's owner-less html_url"
    );
    assert_eq!(bookmark.title, "A handy script");
    assert_eq!(
        bookmark.note,
        "<blockquote>A  handy\nscript</blockquote>\n\nFiles: a.py, b.rs, c.rs, notes"
    );
    assert_eq!(
        bookmark.tags,
        [
            "github-star",
            "github-gist",
            "lang:python",
            "lang:rust",
            "author:github:Someone"
        ]
    );
    assert_eq!(bookmark.timestamp, None, "`now` is the default post date");
    assert_eq!(draft.dedup_key, format!("gist.github.com/{ID}"));
}

#[test]
fn a_gist_without_a_description_is_titled_by_its_first_file() {
    for description in [None, Some(""), Some("  ")] {
        let gist = two_file_gist(description);
        assert_eq!(gist.title(), "a.py", "{description:?}");
        assert_eq!(gist.note(), "Files: a.py, b.rs, c.rs, notes");
    }
    let empty = gist(json!({ "id": ID, "html_url": format!("https://gist.github.com/{ID}") }));
    assert_eq!(empty.title(), ID);
    assert_eq!(empty.note(), "");
}

#[test]
fn an_anonymous_gist_keeps_the_apis_url_and_gets_no_owner_tag() {
    let draft = gist(json!({
        "id": ID,
        "html_url": format!("https://gist.github.com/{ID}"),
        "files": { "a.sh": null },
    }))
    .into_draft(&default_config())
    .unwrap();
    assert_eq!(
        draft.bookmark.url.as_str(),
        format!("https://gist.github.com/{ID}")
    );
    assert_eq!(draft.bookmark.tags, ["github-star", "github-gist"]);
    assert_eq!(draft.bookmark.title, "a.sh");
}

#[test]
fn a_gist_with_an_unparseable_url_makes_no_draft() {
    let anonymous = gist(json!({ "id": ID, "html_url": "not a url" }));
    assert!(anonymous.into_draft(&default_config()).is_none());
}

#[test]
fn the_gist_post_date_picks_created_or_updated() {
    let dated = |post_date| {
        let cfg = GistConfig {
            post_date,
            ..default_config()
        };
        two_file_gist(None)
            .into_draft(&cfg)
            .unwrap()
            .bookmark
            .timestamp
    };
    assert_eq!(dated(GistPostDate::Now), None);
    assert_eq!(
        dated(GistPostDate::Created),
        crate::timefmt::parse_rfc3339("2020-01-02T03:04:05Z")
    );
    assert_eq!(
        dated(GistPostDate::Updated),
        crate::timefmt::parse_rfc3339("2024-05-06T07:08:09Z")
    );
}

#[test]
fn gist_tags_follow_their_own_config_and_an_empty_prefix_disables_one() {
    let cfg = GistConfig {
        tags: vec!["gist".into(), "".into()],
        lang_prefix: "".into(),
        author_prefix: "by:".into(),
        ..default_config()
    };
    let draft = two_file_gist(None).into_draft(&cfg).unwrap();
    assert_eq!(draft.bookmark.tags, ["gist", "by:Someone"]);
}

#[test]
fn gist_config_inherits_the_stars_tags_with_github_gist_added() {
    let tags = |list: &[&str]| list.iter().map(|tag| tag.to_string()).collect::<Vec<_>>();
    let inherited = GistConfig::inheriting(&tags(&["star", "extra"]), "l:", "a:");
    assert_eq!(inherited.tags, tags(&["star", "extra", "github-gist"]));
    assert_eq!(inherited.lang_prefix, "l:");
    assert_eq!(inherited.author_prefix, "a:");
    assert!(inherited.enabled);
    assert_eq!(inherited.post_date, GistPostDate::Now);
    assert_eq!(
        GistConfig::inheriting(&tags(&["github-gist"]), "l:", "a:").tags,
        tags(&["github-gist"]),
        "not twice"
    );
}

#[test]
fn gist_root_finds_the_id_and_any_owner() {
    let root = |s: &str| gist_root(&url(s)).map(|(owner, id)| (owner.map(str::to_string), id));
    assert_eq!(
        root(&format!("https://gist.github.com/{ID}")),
        Some((None, ID.into()))
    );
    assert_eq!(
        root(&format!("https://gist.github.com/Someone/{ID}.git/")),
        Some((Some("Someone".into()), ID.into()))
    );
    assert_eq!(
        root("https://gist.github.com/someone/0123456789abcdef0123456789ABCDEF"),
        Some((
            Some("someone".into()),
            "0123456789abcdef0123456789abcdef".into()
        ))
    );
    assert_eq!(
        root("https://gist.github.com/42"),
        Some((None, "42".into()))
    );
    for not_a_root in [
        "https://gist.github.com/someone".to_string(),
        "https://gist.github.com/deadbeef".to_string(),
        "https://gist.github.com/starred".to_string(),
        format!("https://gist.github.com/someone/{ID}/0123abc"),
        format!("https://gist.githubusercontent.com/someone/{ID}/raw"),
        format!("https://github.com/someone/{ID}"),
        "https://gist.github.com/".to_string(),
    ] {
        assert_eq!(root(&not_a_root), None, "{not_a_root}");
    }
}

#[test]
fn gist_post_date_parses_its_three_values_and_never_quotes_a_bad_one() {
    let parse = |text: &str| text.parse::<GistPostDate>();
    assert_eq!(parse("now"), Ok(GistPostDate::Now));
    assert_eq!(parse("created"), Ok(GistPostDate::Created));
    assert_eq!(parse(" updated "), Ok(GistPostDate::Updated));
    let error = parse("ghp_SECRET").unwrap_err();
    assert!(error.contains("`gist_post_date`"), "{error}");
    assert!(!error.contains("SECRET"), "{error}");
}
