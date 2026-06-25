//! The service-agnostic bookmark domain type. [`Bookmark`] is what the `cleanup`
//! driver reads (from Pinboard) and plans an end-state in, independent of any service's
//! wire format: tags split out, the creation time as a real [`OffsetDateTime`], and the
//! flags as plain `bool`s. The Pinboard wire shape it's converted `From` lives next to
//! the client in [`crate::pinboard::PinboardBookmark`]; the formatting back to Pinboard
//! fields happens at the write boundary in `pinboard::post_add`.

use std::time::Duration;

use anyhow::{Context, Result};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

use crate::pinboard::{PinboardBookmark, RATE_LIMIT_SECS};

/// A bookmark in service-agnostic domain form.
#[derive(Debug, Clone)]
pub struct Bookmark {
    pub url: String,
    pub description: String,
    pub extended: String,
    pub tags: Vec<String>,
    /// Creation time, or `None` when none was set / it didn't parse.
    pub timestamp: Option<OffsetDateTime>,
    /// Whether the bookmark is public (Pinboard's `shared=yes`).
    pub public: bool,
    /// Whether the bookmark is queued to read (Pinboard's `toread=yes`).
    pub read_later: bool,
}

impl From<PinboardBookmark> for Bookmark {
    fn from(b: PinboardBookmark) -> Self {
        Bookmark {
            url: b.url,
            description: b.description,
            extended: b.extended,
            tags: b.tags.split_whitespace().map(String::from).collect(),
            timestamp: OffsetDateTime::parse(&b.time, &Rfc3339).ok(),
            public: b.shared == "yes",
            read_later: b.toread == "yes",
        }
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
    async fn delete(&self, url: &str) -> Result<()>;
    /// Seconds to pause between successive writes (Pinboard asks for ~3s).
    fn rate_limit_secs(&self) -> u64 {
        RATE_LIMIT_SECS
    }
}

/// The shared write step of the cleanup loops: rate-limit after the first write (so
/// successive `posts/add`s are spaced), `update` the bookmark, then `delete` the old
/// URL when it changed (`old_url`). `wrote` gates the inter-write delay and is set
/// once any write has happened.
pub async fn apply_update<P: BookmarkStore>(
    pinboard: &P,
    wrote: &mut bool,
    update: &Bookmark,
    old_url: Option<&str>,
) -> Result<()> {
    if *wrote {
        tokio::time::sleep(Duration::from_secs(pinboard.rate_limit_secs())).await;
    }
    pinboard
        .update(update)
        .await
        .with_context(|| format!("updating bookmark {}", update.url))?;
    if let Some(old) = old_url {
        pinboard
            .delete(old)
            .await
            .with_context(|| format!("deleting old URL {old}"))?;
    }
    *wrote = true;
    Ok(())
}
