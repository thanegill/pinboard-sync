use super::*;
use crate::test_support::{stored_at, strings};
use serde_json::json;

const ID: &str = "aa5a315d61ae9438b18d";

fn gist(description: Option<&str>) -> GitHubGist {
    serde_json::from_value(json!({
        "id": ID,
        "html_url": format!("https://gist.github.com/{ID}"),
        "description": description,
        "files": { "b.rs": { "language": "Rust" }, "a.md": { "language": "Markdown" } },
        "owner": { "login": "someone" },
    }))
    .unwrap()
}

fn config(tags: &[&str], gist_tags: &[&str]) -> GitHubConfig {
    let mut cfg = GitHubConfig {
        tags: strings(tags),
        ..GitHubConfig::default()
    };
    cfg.gists.tags = strings(gist_tags);
    cfg
}

#[test]
fn the_markers_are_the_first_gist_tag_and_the_first_the_stars_lack() {
    let markers = |cfg: GitHubConfig| markers(&cfg).into_iter().collect::<Vec<_>>();
    assert_eq!(
        markers(GitHubConfig::default()),
        strings(&["github-gist", "github-star"])
    );
    assert_eq!(
        markers(config(&["star"], &["", "star", "gist", "extra"])),
        strings(&["gist", "star"]),
        "an extra tag isn't one, so adding it later still reaches synced gists"
    );
    assert_eq!(
        markers(config(&["star"], &["", "gist", "star"])),
        strings(&["gist"])
    );
    assert_eq!(markers(config(&["star"], &["star"])), strings(&["star"]));
    assert!(markers(config(&["star"], &["", ""])).is_empty());
}

#[test]
fn gist_configs_keep_one_config_per_marker_set_the_clients_first() {
    let mut own = config(&["github-star"], &["github-star", "github-gist"]);
    own.gists.lang_prefix = "own:".into();
    let mut other = own.clone();
    other.gists.lang_prefix = "other:".into();
    other.gists.tags = strings(&["github-star", "github-gist", "extra"]);
    let work = config(&["work-star"], &["work-star", "github-gist"]);
    let off = config(&["x"], &[""]);
    let kept: Vec<String> = gist_configs(&own, &[other, off, work])
        .into_iter()
        .map(|(_, cfg)| cfg.lang_prefix)
        .collect();
    assert_eq!(kept, ["own:", "lang:"]);
}

/// Two accounts with default gist tags share `github-gist`, so a gist is synced under the
/// one whose whole set it carries, and only that one refreshes it.
#[test]
fn two_default_accounts_each_own_only_their_gists() {
    let home = GitHubConfig::resolve(None, &Default::default());
    let work_account = crate::config::GitHubAccount {
        tags: Some(strings(&["work-star"])),
        ..Default::default()
    };
    let work = GitHubConfig::resolve(Some(&work_account), &Default::default());
    let configs = gist_configs(&home, &[home.clone(), work]);
    let owners = |tags: &[&str]| -> Vec<Vec<String>> {
        let tags = strings(tags);
        configs
            .iter()
            .filter(|(markers, _)| synced_under(markers, &tags))
            .map(|(_, cfg)| cfg.tags.clone())
            .collect()
    };
    assert_eq!(
        owners(&["work-star", "github-gist", "lang:rust"]),
        [strings(&["work-star", "github-gist"])]
    );
    assert_eq!(
        owners(&["github-star", "github-gist"]),
        [strings(&["github-star", "github-gist"])]
    );
    assert!(owners(&["github-star"]).is_empty(), "saved by hand");
    assert!(owners(&["github-gist"]).is_empty());
}

#[test]
fn tool_shaped_gist_notes_are_recognized_on_any_bookmark() {
    let current = gist(Some("Fresh"));
    for note in [
        "",
        " \n",
        "<blockquote>Fresh</blockquote>",
        "<blockquote>Fresh</blockquote>\n\nFiles: a.md, b.rs",
        "Files: a.md, b.rs",
    ] {
        for synced in [true, false] {
            assert!(
                note_is_tool_generated(note, &current, synced),
                "{note:?} is tool-shaped"
            );
        }
    }
}

#[test]
fn a_stale_gist_quote_or_file_list_is_the_tools_only_on_a_synced_gist() {
    let current = gist(Some("Fresh"));
    for note in [
        "<blockquote>Older</blockquote>",
        "<blockquote>Older</blockquote>\n\nFiles: a.md",
        "<blockquote>Fresh</blockquote>\n\nFiles: old.py",
        "Files: old.py",
    ] {
        assert!(note_is_tool_generated(note, &current, true), "{note:?}");
        assert!(!note_is_tool_generated(note, &current, false), "{note:?}");
    }
}

#[test]
fn other_gist_notes_are_kept() {
    let current = gist(Some("Fresh"));
    for note in [
        "my notes",
        "Fresh",
        "HN Link: https://news.ycombinator.com/item?id=42",
        "<blockquote>Fresh</blockquote>\n\nmy own notes",
        "<blockquote>Fresh</blockquote>\n\nFiles: a.md\nmore of my notes",
        "Files: a.md\nmore",
        "<blockquote>a</blockquote> and <blockquote>b</blockquote>",
    ] {
        assert!(!note_is_tool_generated(note, &current, true), "{note:?}");
    }
}

#[test]
fn a_synced_gists_title_is_the_tools_while_it_is_one_sync_gave() {
    let current = gist(Some("Fresh  one"));
    let stale_note = "<blockquote>Older  one</blockquote>\n\nFiles: a.md";
    for title in ["", "Fresh one", "a.md", "b.rs", "Older one"] {
        assert!(
            title_is_tool_generated(title, stale_note, &current),
            "{title:?}"
        );
    }
    for title in ["my title", "Show HN: Fresh one", "Older"] {
        assert!(
            !title_is_tool_generated(title, stale_note, &current),
            "{title:?}"
        );
    }
}

#[test]
fn a_synced_gists_tags_are_refreshed_by_each_config() {
    let home = GitHubConfig::default().gists;
    let mut work = home.clone();
    work.tags = strings(&["work-gist"]);
    work.lang_prefix = "language:".into();
    let existing = strings(&[
        "github-star",
        "github-gist",
        "lang:go",
        "language:go",
        "author:github:old",
        "x",
    ]);
    let mut refreshed = refresh_tags(existing, &gist(None), &[&home, &work]);
    refreshed.sort();
    assert_eq!(
        refreshed,
        strings(&[
            "author:github:someone",
            "github-gist",
            "github-star",
            "lang:markdown",
            "lang:rust",
            "language:markdown",
            "language:rust",
            "work-gist",
            "x",
        ])
    );
}

/// The two passes split by host, so the inherited `github-star` on a gist can't make the
/// repo pass take it for a star, and a star can't be taken for a gist: each skips the
/// other's bookmarks before any lookup.
#[tokio::test]
async fn neither_pass_plans_the_others_bookmarks() {
    let client = GitHubClient::with_base_url(
        "tok".into(),
        GitHubConfig::default(),
        "http://unused.invalid".into(),
    );
    let star = Bookmark {
        tags: strings(&["github-star", "github-gist"]),
        ..stored_at("https://github.com/o/r")
    };
    let synced_gist = Bookmark {
        tags: strings(&["github-star", "github-gist"]),
        ..stored_at(&format!("https://gist.github.com/someone/{ID}"))
    };

    let gist_pass = GistCleanupPass {
        client: &client,
        starred: std::collections::HashMap::new(),
        configs: gist_configs(&client.config, &[]),
    };
    let (plan, waits) = crate::http::recording_waits(gist_pass.plan(&star)).await;
    assert!(matches!(plan, Ok(Plan::Skipped)));
    assert!(waits.is_empty(), "the gist pass looked a repo up");

    let repo_pass = super::super::GitHubCleanupPass {
        client: &client,
        star_dates: std::collections::HashMap::new(),
        star_configs: super::super::star_configs(&client.config, &[]),
    };
    let (plan, waits) = crate::http::recording_waits(repo_pass.plan(&synced_gist)).await;
    assert!(matches!(plan, Ok(Plan::Skipped)));
    assert!(waits.is_empty(), "the repo pass looked a gist up");
}
