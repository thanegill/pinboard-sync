//! The service-agnostic bookmark domain type. [`Bookmark`] is what the `cleanup`
//! driver reads (from Pinboard) and plans an end-state in, independent of any service's
//! wire format: tags split out, the creation time as a real [`OffsetDateTime`], and the
//! flags as plain `bool`s. The Pinboard wire shape it's converted `From` lives next to
//! the client in [`crate::pinboard::PinboardBookmark`]; the formatting back to Pinboard
//! fields happens at the write boundary in `pinboard::post_add`.

use anyhow::{Context, Result};
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
    /// Creation time, or `None` when none was set / it didn't parse.
    pub timestamp: Option<OffsetDateTime>,
    /// Whether the bookmark is public (Pinboard's `shared=yes`).
    pub public: bool,
    /// Whether the bookmark is queued to read (Pinboard's `toread=yes`).
    pub read_later: bool,
}

impl TryFrom<PinboardBookmark> for Bookmark {
    /// Fails only when the wire `href` doesn't parse as a URL; `all()` skips (and warns
    /// on) such entries.
    type Error = url::ParseError;
    fn try_from(b: PinboardBookmark) -> Result<Self, Self::Error> {
        Ok(Bookmark {
            url: Url::parse(&b.url)?,
            title: b.description,
            note: b.extended,
            tags: b.tags.split_whitespace().map(String::from).collect(),
            timestamp: crate::timefmt::parse_rfc3339(&b.time),
            public: b.shared == "yes",
            read_later: b.toread == "yes",
        })
    }
}

impl Bookmark {
    /// The written fields where `new` differs from `self` (the stored bookmark), each as
    /// a `(label, rendered new value)` pair for the cleanup dry-run. Empty when nothing a
    /// write would change differs — so `cleanup` skips the bookmark. `timestamp` compares
    /// by instant (a re-formatted but equivalent time isn't a change).
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
        if new.public != self.public {
            changes.push(("public", new.public.to_string()));
        }
        if new.read_later != self.read_later {
            changes.push(("toread", new.read_later.to_string()));
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
    fn diff_reports_public_change() {
        let stored = bookmark();
        let planned = Bookmark {
            public: true,
            ..bookmark()
        };
        assert_eq!(stored.diff(&planned), vec![("public", "true".to_string())]);
    }

    #[test]
    fn diff_reports_read_later_change() {
        let stored = bookmark();
        let planned = Bookmark {
            read_later: true,
            ..bookmark()
        };
        assert_eq!(stored.diff(&planned), vec![("toread", "true".to_string())]);
    }

    #[test]
    fn diff_ignores_unchanged_privacy_flags() {
        let stored = bookmark();
        assert!(stored.diff(&bookmark()).is_empty());
    }
}
