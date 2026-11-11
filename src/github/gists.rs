//! Starred gists (`/gists/starred`), shaped like a repo star and keyed by id, which both
//! `gist.github.com/<id>` and `gist.github.com/<owner>/<id>` carry.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use log::warn;
use serde::Deserialize;
use url::Url;

use crate::bookmark::Bookmark;
use crate::github::without_git_suffixes;
use crate::htmltext::{blockquote, collapse_whitespace};
use crate::keys::GitHubKeys;
use crate::source::{BookmarkDraft, UrlKey};
use crate::tags::{prefixed, Tags};

/// The host gists live on.
pub const GIST_HOST: &str = "gist.github.com";

/// The base tag `gist_tags` adds to the account's `tags` by default.
const GIST_TAG: &str = "github-gist";

/// Which of a gist's dates is its post date. Gists have no starred-at date, so by default a
/// gist is dated when synced.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum GistPostDate {
    #[default]
    Now,
    Created,
    Updated,
}

/// The error for a malformed `gist_post_date`, naming the field, never the value: it could
/// be a credential pasted into the wrong key.
const GIST_POST_DATE_EXPECTED: &str = "`gist_post_date` must be `now`, `created` or `updated`";

impl FromStr for GistPostDate {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        match text.trim() {
            "now" => Ok(Self::Now),
            "created" => Ok(Self::Created),
            "updated" => Ok(Self::Updated),
            _ => Err(GIST_POST_DATE_EXPECTED.to_owned()),
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
/// repo stars' ([`GistConfig::inheriting`]); an empty prefix disables its tag.
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
    /// The repo stars' tags, with `github-gist` added to the base tags.
    pub fn inheriting(tags: &[String], lang_prefix: &str, author_prefix: &str) -> Self {
        let mut gist_tags = tags.to_vec();
        if !gist_tags.iter().any(|tag| tag == GIST_TAG) {
            gist_tags.push(GIST_TAG.to_owned());
        }
        Self {
            enabled: true,
            tags: gist_tags,
            lang_prefix: lang_prefix.to_owned(),
            author_prefix: author_prefix.to_owned(),
            post_date: GistPostDate::Now,
        }
    }
}

/// A gist, as `/gists/starred` and `/gists/{id}` return it.
#[derive(Debug, Clone, Deserialize)]
pub struct GitHubGist {
    pub id: String,
    html_url: String,
    #[serde(default)]
    pub description: Option<String>,
    /// Keyed by filename; a file can be `null`.
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
    /// The filenames, in the order the notes list them and the title picks from.
    pub fn filenames(&self) -> impl Iterator<Item = &str> {
        self.files.keys().map(String::as_str)
    }

    /// The owner's login; an anonymous gist has none.
    pub fn owner(&self) -> Option<&str> {
        self.owner.as_ref().map(|owner| owner.login.as_str())
    }

    /// The `gist.github.com/<owner>/<id>` URL `cleanup github` canonicalizes to, else the
    /// API's `html_url` for an anonymous gist. `None` (warned) when neither parses.
    pub fn canonical_url(&self) -> Option<Url> {
        let canonical = self.owner().map_or_else(
            || self.html_url.clone(),
            |owner| format!("https://{GIST_HOST}/{owner}/{}", self.id),
        );
        Url::parse(&canonical)
            .map_err(|e| warn!("skipping github gist with unparseable URL {canonical}: {e}"))
            .ok()
    }

    /// Its description, else its first file, else its id.
    pub fn title(&self) -> String {
        let description = collapse_whitespace(self.description.as_deref().unwrap_or(""), " ");
        if !description.is_empty() {
            return description;
        }
        self.filenames()
            .next()
            .map_or_else(|| self.id.clone(), str::to_owned)
    }

    /// [`gist_note`] of its description and filenames.
    pub fn note(&self) -> String {
        gist_note(self.description.as_deref(), self.filenames())
    }

    /// A language tag per distinct file language, in file order, then the owner tag.
    pub fn derived_tags(&self, cfg: &GistConfig) -> Tags {
        let mut tags = Tags::default();
        let languages = self
            .files
            .values()
            .flatten()
            .filter_map(|file| file.language.as_deref())
            .filter(|language| !language.is_empty());
        for language in languages {
            if let Some(tag) = prefixed(&cfg.lang_prefix, &language.to_lowercase()) {
                tags.push(&tag);
            }
        }
        if let Some(owner) = self.owner() {
            tags.push_prefixed(&cfg.author_prefix, owner);
        }
        tags
    }

    /// The post date `post_date` picks, at whole UTC seconds as a star's, or `None` for
    /// [`GistPostDate::Now`] or an unparseable date.
    pub fn post_date(&self, post_date: GistPostDate) -> Option<time::OffsetDateTime> {
        let date = match post_date {
            GistPostDate::Now => return None,
            GistPostDate::Created => self.created_at.as_deref(),
            GistPostDate::Updated => self.updated_at.as_deref(),
        };
        date.and_then(crate::timefmt::parse_rfc3339)
            .and_then(crate::timefmt::utc_whole_seconds)
    }

    /// The draft, or `None` (with a warning) when its URL doesn't parse.
    pub fn into_draft(self, cfg: &GistConfig) -> Option<BookmarkDraft> {
        let url = self.canonical_url()?;
        let dedup_key = GitHubKeys
            .dedup_key(&url)
            .unwrap_or_else(|| url.as_str().into());
        let mut tags = Tags::default();
        tags.push_all(&cfg.tags);
        tags.push_all(&self.derived_tags(cfg));
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

/// A gist's notes, for sync and cleanup alike: its `description` in a `<blockquote>`, which
/// Pinboard renders, then a `Files:` line.
pub fn gist_note<'a>(
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

/// Whether `url` is on the gist host, bare or with the `www.` alias that `github.com` has.
pub fn is_gist_host(url: &Url) -> bool {
    url.host_str()
        .is_some_and(|host| host.strip_prefix("www.").unwrap_or(host) == GIST_HOST)
}

/// The id of a gist-root URL (`gist.github.com/<id>` or `gist.github.com/<owner>/<id>`, on
/// an [`is_gist_host`] host, empty segments and trailing `.git`s ignored as for a repo
/// name), or `None` for a deep link, an owner's gist list or another host.
pub fn gist_root_id(url: &Url) -> Option<String> {
    if !is_gist_host(url) {
        return None;
    }
    let segments: Vec<&str> = url.path_segments()?.filter(|s| !s.is_empty()).collect();
    let ([id] | [_, id]) = segments[..] else {
        return None;
    };
    let id = without_git_suffixes(id);
    (is_gist_id(id) && !is_commit_sha(id)).then(|| id.to_ascii_lowercase())
}

/// Whether `segment` is a commit SHA, 40 hex characters. No gist id is that long, so
/// neither `gist.github.com/<id>/<sha>` nor a lone SHA is a root.
fn is_commit_sha(segment: &str) -> bool {
    segment.len() == 40 && segment.chars().all(|c| c.is_ascii_hexdigit())
}

/// Whether `segment` is a gist id rather than a username: hex, and all digits (the oldest
/// ids) or at least 20 characters, which leaves out short hex usernames like `cafe`. An
/// all-digit username's gist list passes, and `cleanup github`'s lookup of it finds no gist
/// and leaves it.
fn is_gist_id(segment: &str) -> bool {
    !segment.is_empty()
        && segment.chars().all(|c| c.is_ascii_hexdigit())
        && (segment.chars().all(|c| c.is_ascii_digit()) || segment.len() >= 20)
}

#[cfg(test)]
mod tests;
