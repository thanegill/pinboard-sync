//! Starred gists: a second stream of the GitHub source (`/gists/starred`), shaped like a
//! repo star. A gist is keyed and canonicalized by its id, which both of its URL forms,
//! `gist.github.com/<id>` and `gist.github.com/<owner>/<id>`, carry.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use log::warn;
use serde::Deserialize;
use url::Url;

use crate::bookmark::Bookmark;
use crate::htmltext::{blockquote, collapse_whitespace};
use crate::keys::GitHubKeys;
use crate::source::{BookmarkDraft, UrlKey};
use crate::tags::{prefixed, Tags};

/// The host gists live on.
pub(crate) const GIST_HOST: &str = "gist.github.com";

/// The base tag `gist_tags` adds to the account's `tags` by default.
const GIST_TAG: &str = "github-gist";

/// Which of a gist's dates is its post date. There is no starred-at date for gists, so the
/// default dates a gist by when it was synced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GistPostDate {
    #[default]
    Now,
    Created,
    Updated,
}

/// The complaint about a malformed `gist_post_date`, naming the field and never the
/// value, which could be a credential pasted into the wrong key.
const GIST_POST_DATE_EXPECTED: &str = "`gist_post_date` must be `now`, `created` or `updated`";

impl FromStr for GistPostDate {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.trim() {
            "now" => Ok(Self::Now),
            "created" => Ok(Self::Created),
            "updated" => Ok(Self::Updated),
            _ => Err(GIST_POST_DATE_EXPECTED.to_string()),
        }
    }
}

impl<'de> Deserialize<'de> for GistPostDate {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct GistPostDateVisitor;

        impl serde::de::Visitor<'_> for GistPostDateVisitor {
            type Value = GistPostDate;

            fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                formatter.write_str("`gist_post_date` as `now`, `created` or `updated`")
            }

            fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<GistPostDate, E> {
                text.parse().map_err(E::custom)
            }
        }

        deserializer.deserialize_str(GistPostDateVisitor)
    }
}

impl fmt::Display for GistPostDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Now => "now",
            Self::Created => "created",
            Self::Updated => "updated",
        })
    }
}

/// How an account's starred gists are synced and tagged. Each tag field defaults to the
/// repo stars' own ([`GistConfig::inheriting`]); an empty prefix disables that tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GistConfig {
    /// Whether `sync github` reads the starred gists at all.
    pub enabled: bool,
    pub tags: Vec<String>,
    pub lang_prefix: String,
    pub author_prefix: String,
    pub post_date: GistPostDate,
}

impl GistConfig {
    /// The defaults for an account whose repo stars are tagged `tags`, `lang_prefix` and
    /// `author_prefix`: the same, with `github-gist` added to the base tags.
    pub fn inheriting(tags: &[String], lang_prefix: &str, author_prefix: &str) -> Self {
        let mut gist_tags = tags.to_vec();
        if !gist_tags.iter().any(|tag| tag == GIST_TAG) {
            gist_tags.push(GIST_TAG.to_string());
        }
        Self {
            enabled: true,
            tags: gist_tags,
            lang_prefix: lang_prefix.to_string(),
            author_prefix: author_prefix.to_string(),
            post_date: GistPostDate::Now,
        }
    }
}

/// A gist, as `/gists/starred` and `/gists/{id}` return it.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct GitHubGist {
    pub(crate) id: String,
    html_url: String,
    #[serde(default)]
    pub(crate) description: Option<String>,
    /// Keyed by filename. A file can be `null` in some responses.
    #[serde(default)]
    files: BTreeMap<String, Option<GitHubGistFile>>,
    #[serde(default)]
    owner: Option<GitHubGistOwner>,
    #[serde(default)]
    created_at: Option<String>,
    #[serde(default)]
    updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubGistFile {
    #[serde(default)]
    language: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct GitHubGistOwner {
    login: String,
}

impl GitHubGist {
    /// The gist's filenames, in the order the notes list them and the title picks from.
    pub(crate) fn filenames(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// The owner's login, if the gist has one (an anonymous gist has none).
    pub(crate) fn owner(&self) -> Option<&str> {
        self.owner.as_ref().map(|owner| owner.login.as_str())
    }

    /// The gist's URL in the `gist.github.com/<owner>/<id>` form `cleanup github`
    /// canonicalizes to, falling back to the API's `html_url` (which omits the owner)
    /// for an anonymous gist. `None` (with a warning) if neither parses.
    pub(crate) fn canonical_url(&self) -> Option<Url> {
        let canonical = match self.owner() {
            Some(owner) => format!("https://{GIST_HOST}/{owner}/{}", self.id),
            None => self.html_url.clone(),
        };
        Url::parse(&canonical)
            .map_err(|e| warn!("skipping github gist with unparseable URL {canonical}: {e}"))
            .ok()
    }

    /// The title `sync` gives the gist: its description, else its first file, else its id.
    pub(crate) fn title(&self) -> String {
        let description = collapse_whitespace(self.description.as_deref().unwrap_or(""), " ");
        if !description.is_empty() {
            return description;
        }
        self.filenames()
            .next()
            .map_or_else(|| self.id.clone(), str::to_string)
    }

    /// The notes `sync` gives the gist: [`gist_note`] of its description and filenames.
    pub(crate) fn note(&self) -> String {
        gist_note(self.description.as_deref(), self.filenames())
    }

    /// The derived tags under `cfg`: a `lang:` tag for each distinct file language, in
    /// file order, then the owner tag.
    pub(crate) fn derived_tags(&self, cfg: &GistConfig) -> Tags {
        let mut tags = Tags::default();
        let languages = self
            .files
            .values()
            .flatten()
            .filter_map(|file| file.language.as_deref())
            .filter(|language| !language.is_empty());
        for language in languages {
            if let Some(tag) = prefixed(&cfg.lang_prefix, &language.to_lowercase()) {
                tags.push_unique(&tag);
            }
        }
        if let Some(owner) = self.owner() {
            tags.push_prefixed(&cfg.author_prefix, owner);
        }
        tags
    }

    /// The gist's post date under `post_date`, in UTC at whole seconds as a star is dated.
    /// `None` for [`GistPostDate::Now`] or a date that doesn't parse.
    pub(crate) fn post_date(&self, post_date: GistPostDate) -> Option<time::OffsetDateTime> {
        let date = match post_date {
            GistPostDate::Now => return None,
            GistPostDate::Created => self.created_at.as_deref(),
            GistPostDate::Updated => self.updated_at.as_deref(),
        };
        date.and_then(crate::timefmt::parse_rfc3339)
            .and_then(crate::timefmt::utc_whole_seconds)
    }

    /// Shape the gist into a Pinboard draft under `cfg`. `None` (with a warning) if its
    /// URL doesn't parse.
    pub(crate) fn into_draft(self, cfg: &GistConfig) -> Option<BookmarkDraft> {
        let url = self.canonical_url()?;
        let dedup_key = GitHubKeys
            .dedup_key(&url)
            .unwrap_or_else(|| url.as_str().into());
        let mut tags = Tags::default();
        tags.push_all(&cfg.tags);
        tags.extend_unique(&self.derived_tags(cfg));
        Some(BookmarkDraft {
            bookmark: Bookmark {
                url,
                title: self.title(),
                note: self.note(),
                tags,
                timestamp: self.post_date(cfg.post_date),
                public: false,
                read_later: false,
            },
            dedup_key,
        })
    }
}

/// The notes for a gist: its `description` in a `<blockquote>` (Pinboard renders HTML in
/// the notes), then a `Files:` line listing `filenames`. Shared by sync and cleanup so
/// both produce the same shape.
pub(crate) fn gist_note<'a>(
    description: Option<&str>,
    filenames: impl IntoIterator<Item = &'a str>,
) -> String {
    let files: Vec<&str> = filenames.into_iter().collect();
    let parts = [
        description
            .map(str::trim)
            .filter(|description| !description.is_empty())
            .map(blockquote),
        (!files.is_empty()).then(|| format!("Files: {}", files.join(", "))),
    ];
    parts.into_iter().flatten().collect::<Vec<_>>().join("\n\n")
}

/// The id of a gist-root URL: `gist.github.com/<id>` or `gist.github.com/<owner>/<id>`,
/// ignoring a trailing slash and a `.git` suffix. `None` for anything else: a deep link (a
/// revision, `/raw`), an owner's gist list (`gist.github.com/<owner>`), or another host.
pub(crate) fn gist_root_id(url: &Url) -> Option<String> {
    if url.host_str() != Some(GIST_HOST) {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let ([id] | [_, id]) = segments[..] else {
        return None;
    };
    let id = id.strip_suffix(".git").unwrap_or(id);
    is_gist_id(id).then(|| id.to_ascii_lowercase())
}

/// Whether `segment` is a gist id rather than a username: hex, and either all digits (the
/// oldest gists' numeric ids) or at least 20 characters (20 or 32 since then), which
/// leaves out short all-hex usernames like `cafe`. An all-digit username's gist list
/// passes for an id; `cleanup github`'s lookup of it then finds no gist and leaves it.
fn is_gist_id(segment: &str) -> bool {
    !segment.is_empty()
        && segment.chars().all(|c| c.is_ascii_hexdigit())
        && (segment.chars().all(|c| c.is_ascii_digit()) || segment.len() >= 20)
}

#[cfg(test)]
mod tests;
