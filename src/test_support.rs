//! In-memory fakes implementing the client ports ([`SavedSource`] /
//! [`BookmarkStore`]) so the `sync` and `cleanup` loops can be tested without
//! any network. Compiled only under `#[cfg(test)]`.

use std::cell::RefCell;
use std::collections::HashSet;

use anyhow::{anyhow, Result};
use serde_json::Value;
use url::Url;

use crate::bookmark::{Bookmark, BookmarkStore};
use crate::model::{reddit_key, RedditConfig, RedditListingEntry};
use crate::reddit::PostInfo;
use crate::source::{BookmarkDraft, Source, SourceError, UrlKey};

/// Build a `RedditListingEntry` from `kind` (`t3`/`t1`) and a `data` JSON object.
pub fn listing_entry(kind: &str, data: Value) -> RedditListingEntry {
    serde_json::from_value(serde_json::json!({ "kind": kind, "data": data })).unwrap()
}

#[derive(Default)]
pub struct FakeReddit {
    pub saved: Vec<RedditListingEntry>,
    pub info: Vec<RedditListingEntry>,
    /// When set, `info` fails with it instead of answering — for exercising the
    /// expired-cookie path that has to reach the auth-failure hook.
    pub info_error: Option<SourceError>,
}

impl Source for FakeReddit {
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError> {
        let cfg = RedditConfig::default();
        Ok(self
            .saved
            .iter()
            .cloned()
            .filter_map(|e| e.into_saved_item(&cfg.domain))
            .filter_map(|it| it.into_draft(&cfg))
            .collect())
    }
}

impl UrlKey for FakeReddit {
    fn dedup_key(&self, url: &Url) -> Option<String> {
        reddit_key(url)
    }
}

impl PostInfo for FakeReddit {
    async fn info(&self, _fullnames: &[String]) -> Result<Vec<RedditListingEntry>, SourceError> {
        match &self.info_error {
            Some(SourceError::ReauthRequired(m)) => Err(SourceError::ReauthRequired(m.clone())),
            Some(SourceError::RateLimited(m)) => Err(SourceError::RateLimited(m.clone())),
            Some(SourceError::Other(e)) => Err(SourceError::Other(anyhow::anyhow!("{e}"))),
            None => Ok(self.info.clone()),
        }
    }
}

/// The recorded `dt` a write would send: the bookmark's timestamp as RFC3339, or empty.
fn bookmark_dt(b: &Bookmark) -> String {
    b.timestamp
        .and_then(crate::timefmt::to_rfc3339)
        .unwrap_or_default()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddCall {
    pub url: String,
    pub tags: Vec<String>,
    pub toread: bool,
    pub shared: bool,
    pub dt: String,
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

/// A call that actually changed the account. `added`/`updated`/`deleted` record every
/// *attempt*; this records only the ones that succeeded, which is what the account can be
/// rebuilt from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Write(Bookmark),
    Delete(Url),
}

/// Rebuild the account from `seed` plus the ops that landed. Deliberately a plain `Vec`
/// rather than an [`crate::bookmark::AccountView`]: a replay built from the type under test
/// would reproduce that type's own bug on both sides of the comparison and prove nothing.
/// Assumes `seed` has no duplicate URLs, as `posts/all` normally does not.
pub fn replay(seed: &[Bookmark], ops: &[Op]) -> Vec<Bookmark> {
    let mut account = seed.to_vec();
    for op in ops {
        match op {
            Op::Write(b) => match account.iter().position(|held| held.url == b.url) {
                Some(at) => account[at] = b.clone(),
                None => account.push(b.clone()),
            },
            Op::Delete(url) => account.retain(|held| held.url != *url),
        }
    }
    account
}

#[derive(Default)]
pub struct FakePinboard {
    pub all: Vec<Bookmark>,
    /// How many times `all()` was called (to assert single-fetch behavior).
    pub all_calls: RefCell<usize>,
    pub added: RefCell<Vec<AddCall>>,
    pub updated: RefCell<Vec<UpdateCall>>,
    pub deleted: RefCell<Vec<String>>,
    /// URLs whose `add` should fail, to exercise log-and-skip behavior.
    pub fail_add_urls: HashSet<String>,
    /// URLs whose `update` should fail, to exercise cleanup log-and-skip behavior.
    pub fail_update_urls: HashSet<String>,
    /// URLs whose `delete` should fail. A merge writes then deletes, so this is the only
    /// way to reach the half-applied case where the write landed and an absorbed URL
    /// survived.
    pub fail_delete_urls: HashSet<String>,
    /// The successful writes and deletes, interleaved in call order — see [`replay`].
    pub ops: RefCell<Vec<Op>>,
}

impl BookmarkStore for FakePinboard {
    async fn all(&self) -> Result<Vec<Bookmark>> {
        *self.all_calls.borrow_mut() += 1;
        Ok(self.all.clone())
    }
    async fn add(&self, b: &Bookmark) -> Result<()> {
        if self.fail_add_urls.contains(b.url.as_str()) {
            return Err(anyhow!("simulated add failure for {}", b.url));
        }
        self.added.borrow_mut().push(AddCall {
            url: b.url.to_string(),
            tags: b.tags.clone(),
            toread: b.read_later,
            shared: b.public,
            dt: bookmark_dt(b),
        });
        self.ops.borrow_mut().push(Op::Write(b.clone()));
        Ok(())
    }
    async fn update(&self, b: &Bookmark) -> Result<()> {
        if self.fail_update_urls.contains(b.url.as_str()) {
            return Err(anyhow!("simulated update failure for {}", b.url));
        }
        self.updated.borrow_mut().push(UpdateCall {
            url: b.url.to_string(),
            description: b.title.clone(),
            extended: b.note.clone(),
            tags: b.tags.clone(),
            shared: b.public,
            toread: b.read_later,
            dt: bookmark_dt(b),
        });
        self.ops.borrow_mut().push(Op::Write(b.clone()));
        Ok(())
    }
    async fn delete(&self, url: &Url) -> Result<()> {
        self.deleted.borrow_mut().push(url.to_string());
        if self.fail_delete_urls.contains(url.as_str()) {
            anyhow::bail!("fake delete failure for {url}");
        }
        self.ops.borrow_mut().push(Op::Delete(url.clone()));
        Ok(())
    }
}
