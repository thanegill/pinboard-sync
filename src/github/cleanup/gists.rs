//! `cleanup github`'s gist pass: canonicalize each gist bookmark to
//! `gist.github.com/<owner>/<id>` and refresh what `sync github` wrote of it, under the
//! same ownership rules as the repo pass.

use std::collections::{BTreeSet, HashMap};

use super::{is_lone_blockquote, retag};
use crate::bookmark::{AccountState, Bookmark, BookmarkStore};
use crate::cleanup_pass::{run_pass, CleanupPass, Plan};
use crate::config::own_then_configured;
use crate::github::gists::{gist_root_id, GistConfig, GitHubGist, GIST_HOST};
use crate::github::{GitHubClient, GitHubConfig};
use crate::htmltext::{blockquote, collapse_whitespace};
use crate::source::SourceError;
use crate::tags::Tags;
use crate::timefmt::DateOpts;

/// Plan every gist bookmark in the run's live view (so the repo pass's writes are already
/// reflected), unless the running account has `gists = false`. `configured` is every
/// configured account's config, as for the repos.
///
/// The starred gists are read once up front and planned from; only a gist not among them
/// (saved by hand, unstarred or deleted) is looked up on its own. A failed read fails the
/// pass before anything is planned, so it can't let every gist look reached, and a halt
/// there (a dead token, a rate limit) comes back as itself.
pub(super) async fn run<S: BookmarkStore + AccountState>(
    store: &S,
    client: &GitHubClient,
    dates: DateOpts,
    configured: &[GitHubConfig],
) -> Result<(), SourceError> {
    if !client.config.gists.enabled {
        return Ok(());
    }
    let gist_bookmarks = store.snapshot(|bookmark| bookmark.url.host_str() == Some(GIST_HOST));
    let starred = if gist_bookmarks
        .iter()
        .any(|bookmark| gist_root_id(&bookmark.url).is_some())
    {
        client.starred_gists().await?
    } else {
        HashMap::new()
    };
    let pass = GistCleanupPass {
        client,
        starred,
        configs: gist_configs(&client.config, configured),
    };
    run_pass(store, &gist_bookmarks, "github gists", dates, &pass)
        .await
        .into_result()
}

/// The tags marking a gist `sync github` wrote under `cfg`, all of which a synced gist
/// carries: the first non-empty entry of its `gist_tags`, and the first that its `tags`
/// lack. No single tag will do, since the default `gist_tags` are the account's `tags`
/// plus `github-gist`: `github-gist` is every account's, so it can't tell one account's
/// gists from another's, and `github-star` also marks a gist saved by hand and tagged
/// like a star. Two rather than every entry, so a tag added to `gist_tags` later still
/// reaches the gists synced before it, as a star's extra tags do. When every entry is also
/// in `tags` the first is the only marker, and a gist tagged like a star then counts.
fn markers(cfg: &GitHubConfig) -> BTreeSet<String> {
    let mut gist_tags = cfg.gists.tags.iter().filter(|tag| !tag.is_empty());
    let first = gist_tags.clone().next();
    let own = gist_tags.find(|tag| !cfg.tags.contains(tag));
    first.into_iter().chain(own).cloned().collect()
}

/// One account's gist config, with the [`markers`] of the gists it synced.
struct MarkedGistConfig {
    markers: BTreeSet<String>,
    config: GistConfig,
}

impl MarkedGistConfig {
    /// Whether a bookmark tagged `tags` is synced under this config.
    fn syncs(&self, tags: &Tags) -> bool {
        self.markers.iter().all(|marker| tags.has(marker))
    }
}

/// The gist configs of `own` followed by each of `configured`, keeping one per set of
/// [`markers`] (the client's own where they share it) and none with no marker at all.
fn gist_configs(own: &GitHubConfig, configured: &[GitHubConfig]) -> Vec<MarkedGistConfig> {
    let mut configs: Vec<MarkedGistConfig> = Vec::new();
    for cfg in own_then_configured(own, configured) {
        let markers = markers(cfg);
        if !markers.is_empty() && !configs.iter().any(|seen| seen.markers == markers) {
            configs.push(MarkedGistConfig {
                markers,
                config: cfg.gists.clone(),
            });
        }
    }
    configs
}

/// Re-shapes one gist bookmark: skip anything that isn't a gist root, else take the gist
/// from the starred list or look it up, and plan its canonical URL and the notes, title,
/// tags and date the tool owns of it. A deleted or refused gist keeps its URL.
struct GistCleanupPass<'a> {
    client: &'a GitHubClient,
    /// The starred gists, keyed by id, from [`GitHubClient::starred_gists`].
    starred: HashMap<String, GitHubGist>,
    /// One per distinct set of marker tags, from [`gist_configs`].
    configs: Vec<MarkedGistConfig>,
}

impl CleanupPass for GistCleanupPass<'_> {
    async fn plan(&self, bookmark: &Bookmark) -> Result<Plan, SourceError> {
        let Some(id) = gist_root_id(&bookmark.url) else {
            return Ok(Plan::Skipped);
        };
        let looked_up;
        let gist = match self.starred.get(&id) {
            Some(gist) => gist,
            None => match self.client.gist(&id).await? {
                Some(gist) => {
                    looked_up = gist;
                    &looked_up
                }
                None => return Ok(Plan::Unchanged),
            },
        };
        let syncing: Vec<&GistConfig> = self
            .configs
            .iter()
            .filter(|marked| marked.syncs(&bookmark.tags))
            .map(|marked| &marked.config)
            .collect();
        let synced = !syncing.is_empty();

        let mut title = collapse_whitespace(&bookmark.title, " ");
        let mut note = bookmark.note.clone();
        let mut tags = bookmark.tags.clone();
        let owns_title = synced && title_is_tool_generated(&title, &bookmark.note, gist);
        let owns_note = note_is_tool_generated(&bookmark.note, gist, synced);
        if owns_note {
            note = gist.note();
        }
        if owns_title {
            title = gist.title();
        }
        if synced {
            tags = refresh_tags(tags, gist, &syncing);
        }
        let timestamp = syncing
            .first()
            .filter(|_| owns_title && owns_note)
            .and_then(|cfg| gist.post_date(cfg.post_date));
        Ok(Plan::Bookmark(Bookmark {
            url: gist.canonical_url().unwrap_or_else(|| bookmark.url.clone()),
            title,
            note,
            tags,
            timestamp,
            public: bookmark.public,
            read_later: bookmark.read_later,
        }))
    }
}

/// `note` split from the file list on the `Files:` line
/// [`gist_note`](crate::github::gists::gist_note) ends with: after a blank line, or as the
/// whole note when the gist has no description.
fn split_files_line(note: &str) -> (&str, Option<&str>) {
    match note.rsplit_once("\n\nFiles: ") {
        Some((body, files)) if !files.contains('\n') => (body, Some(files)),
        _ => match note.strip_prefix("Files: ") {
            Some(files) if !files.contains('\n') => ("", Some(files)),
            _ => (note, None),
        },
    }
}

/// `note` without its `Files:` line ([`split_files_line`]).
fn without_files_line(note: &str) -> &str {
    split_files_line(note).0
}

/// Whether a gist bookmark's stored `note` is one the tool wrote, so a rebuild from the
/// current gist may replace it: empty, or the current description in a `<blockquote>`,
/// either with or without a `Files:` line. On a `synced` gist any lone `<blockquote>` and
/// any file list count, so an upstream edit still refreshes; elsewhere the quote may be a
/// passage the user saved and the list must be the gist's current one.
fn note_is_tool_generated(note: &str, gist: &GitHubGist, synced: bool) -> bool {
    let (body, files) = split_files_line(note);
    let body = body.trim();
    let current_files = gist.filenames().collect::<Vec<_>>().join(", ");
    if !synced && files.is_some_and(|files| files != current_files) {
        return false;
    }
    let description = gist
        .description
        .as_deref()
        .map(str::trim)
        .filter(|description| !description.is_empty());
    body.is_empty()
        || (synced && is_lone_blockquote(body))
        || description.is_some_and(|description| body == blockquote(description))
}

/// Whether a synced gist's (plain-text) `title` is still one sync gave it: empty, the
/// gist's current title, one of its filenames, or the description its stored `note`
/// quotes, from before an upstream edit.
fn title_is_tool_generated(title: &str, note: &str, gist: &GitHubGist) -> bool {
    let title = title.trim();
    let quoted = without_files_line(note)
        .trim()
        .strip_prefix("<blockquote>")
        .and_then(|rest| rest.strip_suffix("</blockquote>"))
        .map(|description| collapse_whitespace(description, " "));
    title.is_empty()
        || title == gist.title()
        || gist.filenames().any(|filename| filename == title)
        || quoted.is_some_and(|description| description == title)
}

/// Refresh a synced gist's language and owner tags under each of `configs`, keeping its
/// other tags and ensuring each config's `gist_tags`, as the repo pass does for a star.
fn refresh_tags(existing: Tags, gist: &GitHubGist, configs: &[&GistConfig]) -> Tags {
    let prefixes: Vec<&str> = configs
        .iter()
        .flat_map(|cfg| [cfg.lang_prefix.as_str(), cfg.author_prefix.as_str()])
        .collect();
    let additions = configs.iter().flat_map(|cfg| {
        let mut tags = Tags::default();
        tags.push_all(&cfg.tags);
        tags.push_all(&gist.derived_tags(cfg));
        tags.to_vec()
    });
    retag(existing, &prefixes, additions)
}

#[cfg(test)]
mod tests;
