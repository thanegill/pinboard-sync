//! In-memory fakes implementing the client ports ([`SavedSource`] /
//! [`BookmarkStore`]) so the `sync` and `cleanup` loops can be tested without
//! any network. Compiled only under `#[cfg(test)]`.

use std::cell::RefCell;

use anyhow::Result;
use serde_json::Value;

use crate::model::{reddit_key, ListingEntry, RedditConfig};
use crate::pinboard::{Bookmark, BookmarkStore};
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
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateCall {
    pub url: String,
    pub description: String,
    pub tags: Vec<String>,
}

#[derive(Default)]
pub struct FakePinboard {
    pub all: Vec<Bookmark>,
    pub added: RefCell<Vec<AddCall>>,
    pub updated: RefCell<Vec<UpdateCall>>,
    pub deleted: RefCell<Vec<String>>,
}

impl BookmarkStore for FakePinboard {
    async fn all(&self) -> Result<Vec<Bookmark>> {
        Ok(self.all.clone())
    }
    async fn add(
        &self,
        url: &str,
        _description: &str,
        _extended: &str,
        tags: &[String],
    ) -> Result<()> {
        self.added.borrow_mut().push(AddCall {
            url: url.to_string(),
            tags: tags.to_vec(),
        });
        Ok(())
    }
    async fn update(
        &self,
        url: &str,
        description: &str,
        _extended: &str,
        tags: &[String],
        _shared: bool,
        _toread: bool,
        _dt: &str,
    ) -> Result<()> {
        self.updated.borrow_mut().push(UpdateCall {
            url: url.to_string(),
            description: description.to_string(),
            tags: tags.to_vec(),
        });
        Ok(())
    }
    async fn delete(&self, url: &str) -> Result<()> {
        self.deleted.borrow_mut().push(url.to_string());
        Ok(())
    }
}
