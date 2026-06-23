//! In-memory fakes implementing the client ports ([`SavedSource`] /
//! [`BookmarkStore`]) so the `sync` and `cleanup` loops can be tested without
//! any network. Compiled only under `#[cfg(test)]`.

use std::cell::RefCell;

use anyhow::Result;
use serde_json::Value;

use crate::model::{reddit_key, ListingEntry, RedditConfig};
use crate::pinboard::{Bookmark, BookmarkStore, BookmarkUpdate};
use crate::reddit::PostInfo;
use crate::source::{BookmarkDraft, Source, SourceError};

/// Build a `ListingEntry` from `kind` (`t3`/`t1`) and a `data` JSON object.
pub fn listing_entry(kind: &str, data: Value) -> ListingEntry {
    serde_json::from_value(serde_json::json!({ "kind": kind, "data": data })).unwrap()
}

#[derive(Default)]
pub struct FakeReddit {
    pub saved: Vec<ListingEntry>,
    pub info: Vec<ListingEntry>,
}

impl Source for FakeReddit {
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError> {
        let cfg = RedditConfig::default();
        Ok(self
            .saved
            .iter()
            .cloned()
            .filter_map(|e| e.into_saved_item(&cfg.domain))
            .map(|it| it.into_draft(&cfg))
            .collect())
    }

    fn existing_key(&self, url: &str) -> Option<String> {
        reddit_key(url)
    }
}

impl PostInfo for FakeReddit {
    async fn info(&self, _fullnames: &[String]) -> Result<Vec<ListingEntry>, SourceError> {
        Ok(self.info.clone())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddCall {
    pub url: String,
    pub tags: Vec<String>,
    pub toread: bool,
    pub shared: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCall {
    pub url: String,
    pub description: String,
    pub extended: String,
    pub tags: Vec<String>,
    pub shared: bool,
    pub toread: bool,
    pub dt: String,
}

#[derive(Default)]
pub struct FakePinboard {
    pub all: Vec<Bookmark>,
    /// How many times `all()` was called (to assert single-fetch behavior).
    pub all_calls: RefCell<usize>,
    pub added: RefCell<Vec<AddCall>>,
    pub updated: RefCell<Vec<UpdateCall>>,
    pub deleted: RefCell<Vec<String>>,
}

impl BookmarkStore for FakePinboard {
    async fn all(&self) -> Result<Vec<Bookmark>> {
        *self.all_calls.borrow_mut() += 1;
        Ok(self.all.clone())
    }
    async fn add(
        &self,
        url: &str,
        _description: &str,
        _extended: &str,
        tags: &[String],
        toread: bool,
        shared: bool,
    ) -> Result<()> {
        self.added.borrow_mut().push(AddCall {
            url: url.to_string(),
            tags: tags.to_vec(),
            toread,
            shared,
        });
        Ok(())
    }
    async fn update(&self, b: BookmarkUpdate<'_>) -> Result<()> {
        self.updated.borrow_mut().push(UpdateCall {
            url: b.url.to_string(),
            description: b.description.to_string(),
            extended: b.extended.to_string(),
            tags: b.tags.to_vec(),
            shared: b.shared,
            toread: b.toread,
            dt: b.dt.to_string(),
        });
        Ok(())
    }
    async fn delete(&self, url: &str) -> Result<()> {
        self.deleted.borrow_mut().push(url.to_string());
        Ok(())
    }
    // No inter-write pacing in tests (skip the real sleep).
    fn rate_limit_secs(&self) -> u64 {
        0
    }
}
