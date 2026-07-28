//! The service-agnostic bookmark domain type. [`Bookmark`] is what the `cleanup`
//! driver reads (from Pinboard) and plans an end-state in, independent of any service's
//! wire format: tags split out, the creation time as a real [`OffsetDateTime`], and the
//! flags as plain `bool`s. The Pinboard wire shape it's converted `From` lives next to
//! the client in [`crate::pinboard::PinboardBookmark`]; the formatting back to Pinboard
//! fields happens at the write boundary in `pinboard::post_add`.

use anyhow::{Context, Result};
use log::warn;
use time::OffsetDateTime;
use url::Url;

use crate::pinboard::PinboardBookmark;
use crate::source::tags_differ;

/// A bookmark in service-agnostic domain form. The field names are the domain's
/// (`title`/`note`/`public`/`read_later`), not Pinboard's wire names
/// (`description`/`extended`/`shared`/`toread` — those stay on [`PinboardBookmark`]). The
/// URL is a parsed [`Url`], so consumers don't re-parse it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Bookmark {
    pub url: Url,
    pub title: String,
    pub note: String,
    pub tags: Vec<String>,
    /// Creation time, or `None` when none was set (an empty wire `time`) or a non-empty
    /// `time` that won't parse (logged on read). It stays `None` rather than dropping the
    /// whole bookmark so the record remains visible to sync's dedup set (see the `TryFrom`
    /// impl below).
    pub timestamp: Option<OffsetDateTime>,
    /// Whether the bookmark is public (Pinboard's `shared=yes`).
    pub public: bool,
    /// Whether the bookmark is queued to read (Pinboard's `toread=yes`).
    pub read_later: bool,
}

impl TryFrom<PinboardBookmark> for Bookmark {
    /// Fails only when the wire `href` doesn't parse as a URL; `all()` skips (and warns
    /// on) such entries. A non-empty `time` that won't parse is *not* fatal: it becomes
    /// `timestamp: None` (logged) so the bookmark stays in the set sync dedups against —
    /// dropping it would make sync re-add the URL and reset its date/title/tags/notes.
    type Error = url::ParseError;
    fn try_from(b: PinboardBookmark) -> Result<Self, Self::Error> {
        let url = Url::parse(&b.url)?;
        let timestamp = if b.time.is_empty() {
            None
        } else {
            let parsed = crate::timefmt::parse_rfc3339(&b.time);
            if parsed.is_none() {
                warn!(
                    "bookmark {url}: unparseable creation time {:?}, dropping date",
                    b.time
                );
            }
            parsed
        };
        Ok(Bookmark {
            url,
            title: b.description,
            note: b.extended,
            tags: b.tags.split_whitespace().map(String::from).collect(),
            timestamp,
            public: b.shared == "yes",
            read_later: b.toread == "yes",
        })
    }
}

impl Bookmark {
    /// The written fields where `new` differs from `self` (the stored bookmark), each as
    /// a `(label, rendered new value)` pair for the cleanup dry-run. Empty when nothing a
    /// write would change differs — so `cleanup` skips the bookmark. `timestamp` compares
    /// by instant (a re-formatted but equivalent time isn't a change). The `public` and
    /// `read_later` flags are not compared: `cleanup` never re-shapes privacy, forcing them
    /// to the stored values before diffing (see `cleanup_pass::run_pass`).
    pub fn diff(&self, new: &Bookmark) -> Vec<(&'static str, String)> {
        let mut changes = Vec::new();
        if new.url != self.url {
            changes.push(("url", new.url.to_string()));
        }
        if new.title != self.title {
            changes.push(("title", new.title.clone()));
        }
        if new.note != self.note {
            let value = if new.note.is_empty() {
                "(removed)".to_string()
            } else {
                new.note.clone()
            };
            changes.push(("notes", value));
        }
        if tags_differ(&self.tags, &new.tags) {
            changes.push(("tags", format!("[{}]", new.tags.join(" "))));
        }
        if new.timestamp != self.timestamp {
            let value = new
                .timestamp
                .and_then(crate::timefmt::to_rfc3339)
                .unwrap_or_default();
            changes.push(("date", value));
        }
        changes
    }
}

/// The bookmark-store operations the sync/cleanup loops depend on. Abstracted from the
/// concrete Pinboard client so those loops can be exercised with an in-memory fake. A
/// write takes a whole [`Bookmark`]; the client maps it to the Pinboard `posts/add`
/// parameters at the boundary (see `pinboard::post_add`).
/// (Crate-internal, never spawned across threads, so the missing `Send` bound from
/// `async fn` in a trait is irrelevant here.)
#[allow(async_fn_in_trait)]
pub trait BookmarkStore {
    /// Every bookmark in the account (`posts/all`).
    async fn all(&self) -> Result<Vec<Bookmark>>;
    /// Add a new bookmark. A `None` `timestamp` lets Pinboard default the date to now.
    async fn add(&self, b: &Bookmark) -> Result<()>;
    /// Re-add an existing bookmark with normalized fields.
    async fn update(&self, b: &Bookmark) -> Result<()>;
    /// Delete a bookmark by URL.
    async fn delete(&self, url: &Url) -> Result<()>;

    /// The cleanup re-write step: `update` the bookmark, then `delete` the old URL when it
    /// changed (`old_url`). Any inter-write pacing is the store's own concern (the Pinboard
    /// client spaces its `posts/add` calls internally).
    async fn apply_update(&self, update: &Bookmark, old_url: Option<&Url>) -> Result<()> {
        self.update(update)
            .await
            .with_context(|| format!("updating bookmark {}", update.url))?;
        if let Some(old) = old_url {
            self.delete(old)
                .await
                .with_context(|| format!("deleting old URL {old}"))?;
        }
        Ok(())
    }

    /// Write a merged bookmark that absorbs one or more colliding bookmarks: `update` the
    /// merged record at its URL, then `delete` every absorbed `old_urls` entry that isn't
    /// the merge target. Deleting the absorbed URLs is what makes a later cleanup run see a
    /// single bookmark at the target and converge.
    async fn apply_merge(&self, update: &Bookmark, old_urls: &[&Url]) -> Result<()> {
        self.update(update)
            .await
            .with_context(|| format!("updating merged bookmark {}", update.url))?;
        for old in old_urls {
            if **old != update.url {
                self.delete(old)
                    .await
                    .with_context(|| format!("deleting absorbed URL {old}"))?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bookmark() -> Bookmark {
        Bookmark {
            url: Url::parse("https://example.com/").unwrap(),
            title: "Title".into(),
            note: "note".into(),
            tags: vec!["a".into(), "b".into()],
            timestamp: None,
            public: false,
            read_later: false,
        }
    }

    #[test]
    fn diff_ignores_privacy_flags() {
        let stored = bookmark();
        let planned = Bookmark {
            public: true,
            read_later: true,
            ..bookmark()
        };
        assert!(stored.diff(&planned).is_empty());
    }

    fn wire(time: &str) -> PinboardBookmark {
        PinboardBookmark {
            url: "https://example.com/".into(),
            description: "T".into(),
            extended: String::new(),
            tags: String::new(),
            time: time.into(),
            shared: "no".into(),
            toread: "no".into(),
        }
    }

    #[test]
    fn unparseable_time_keeps_the_bookmark_with_no_timestamp() {
        // A non-empty `time` that won't parse must NOT drop the record: it stays visible
        // (as `timestamp: None`) so sync's dedup set still contains its URL.
        let bookmark = Bookmark::try_from(wire("not a date")).unwrap();
        assert_eq!(bookmark.url.as_str(), "https://example.com/");
        assert_eq!(bookmark.timestamp, None);
    }

    #[test]
    fn parseable_time_becomes_a_timestamp() {
        let bookmark = Bookmark::try_from(wire("2020-01-01T00:00:00Z")).unwrap();
        assert_eq!(bookmark.timestamp, crate::timefmt::from_unix(1_577_836_800));
    }

    #[test]
    fn unparseable_url_is_the_only_fatal_conversion() {
        let mut bad = wire("");
        bad.url = "not a url".into();
        assert!(Bookmark::try_from(bad).is_err());
    }
}
