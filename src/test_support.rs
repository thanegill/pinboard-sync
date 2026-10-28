//! In-memory fakes implementing the client ports ([`Source`], [`PostInfo`] and
//! [`BookmarkStore`]) so the `sync` and `cleanup` loops can be tested without
//! any network, plus per-test scratch directories, log capture and the sync-then-cleanup
//! round trip. Compiled only under `#[cfg(test)]`.

use std::cell::RefCell;
use std::collections::HashSet;
use std::future::Future;
use std::sync::Once;

use anyhow::{anyhow, Result};
use serde_json::Value;
use tempfile::TempDir;
use time::OffsetDateTime;
use url::Url;

use crate::bookmark::{AccountView, Bookmark, BookmarkStore, CleanupStore, WriteMode};
use crate::config::SyncLimit;
use crate::keys::RedditKeys;
use crate::pinboard::{PinboardBookmark, PinboardClient};
use crate::reddit::model::{RedditConfig, RedditListingEntry};
use crate::reddit::PostInfo;
use crate::source::{BookmarkDraft, DedupKey, Source, SourceError, UrlKey};
use crate::sync::{prepare_new_drafts, write_drafts, JobSettings};
use crate::tags::Tags;
use crate::timefmt::DateOpts;

tokio::task_local! {
    static CAPTURED_LOGS: RefCell<Vec<String>>;
}

/// Records each of this crate's log lines as `"LEVEL message"`, as `-v` would show them,
/// but only from inside [`capturing_logs`], so tests running alongside on other tasks
/// neither see nor disturb each other's lines.
struct CaptureLogger;

impl log::Log for CaptureLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        let crate_name = env!("CARGO_CRATE_NAME");
        let target = metadata.target();
        let ours = target
            .strip_prefix(crate_name)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"));
        ours && CAPTURED_LOGS.try_with(|_| ()).is_ok()
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let line = format!("{} {}", record.level(), record.args());
        let _ = CAPTURED_LOGS.try_with(|lines| lines.borrow_mut().push(line));
    }

    fn flush(&self) {}
}

/// Runs `fut`, returning its output and every line it logged at debug level or above.
pub async fn capturing_logs<T>(fut: impl Future<Output = T>) -> (T, Vec<String>) {
    static INSTALL: Once = Once::new();
    INSTALL.call_once(|| {
        log::set_logger(&CaptureLogger).expect("no other logger in tests");
        log::set_max_level(log::LevelFilter::Debug);
    });
    CAPTURED_LOGS
        .scope(RefCell::default(), async {
            let output = fut.await;
            (output, CAPTURED_LOGS.with(RefCell::take))
        })
        .await
}

/// The warnings among lines [`capturing_logs`] returned.
pub fn warnings(logs: &[String]) -> Vec<&str> {
    logs.iter()
        .map(String::as_str)
        .filter(|line| line.starts_with("WARN"))
        .collect()
}

/// Ends [`long_error_page`], past the excerpt's cut, so it must not reach the message.
pub const ERROR_PAGE_TAIL: &str = "end-of-error-page";

/// A multi-line HTML error page several times longer than an error may quote.
pub fn long_error_page() -> String {
    format!(
        "<html>\n  <head><title>Error</title></head>\n  <body>\n{}  </body>\n\
         {ERROR_PAGE_TAIL}\n</html>\n",
        "    <p>the request could not be served</p>\n".repeat(50)
    )
}

/// `message` is `"{what}: {excerpt}"`, the excerpt of [`long_error_page`]: on one line,
/// within the cap, and cut with the ellipsis.
pub fn assert_quotes_capped_excerpt(message: &str, what: &str) {
    use crate::http::{ELLIPSIS, ERROR_BODY_MAX_BYTES};
    let excerpt = message
        .strip_prefix(&format!("{what}: "))
        .unwrap_or_else(|| panic!("not `{what}: <excerpt>`: {message}"));
    assert!(!excerpt.contains('\n'), "not one line: {message}");
    assert!(
        excerpt.len() <= ERROR_BODY_MAX_BYTES,
        "{} bytes over the cap: {message}",
        excerpt.len()
    );
    assert!(
        excerpt.starts_with("<html> <head><title>Error</title>"),
        "{message}"
    );
    assert!(excerpt.ends_with(ELLIPSIS), "cut not marked: {message}");
    assert!(!excerpt.contains(ERROR_PAGE_TAIL), "not cut: {message}");
}

/// A fresh, empty directory unique to this call, removed when the returned guard is
/// dropped: keep it bound for as long as the test uses the path.
pub fn scratch_dir(label: &str) -> TempDir {
    tempfile::Builder::new()
        .prefix(&format!("pinboard-sync-test-{label}-"))
        .tempdir()
        .unwrap()
}

/// The base URL of a local HTTP/1.1 server whose every response is cut off: it answers
/// with `status` (e.g. `"200 OK"`), `content_type` and `body`, declares a
/// `Content-Length` longer than `body`, and closes the connection. Binds a socket, so only
/// a `net_tests` case may call it.
pub fn truncated_body_server(
    status: &'static str,
    content_type: &'static str,
    body: &'static str,
) -> String {
    truncated_body_server_with(status, vec![("Content-Type", content_type)], body)
}

/// [`truncated_body_server`] answering with every one of `headers`, each a name and a
/// value, where that one sends only a `Content-Type`.
pub fn truncated_body_server_with(
    status: &'static str,
    headers: Vec<(&'static str, &'static str)>,
    body: &'static str,
) -> String {
    use std::io::{Read, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for socket in listener.incoming() {
            let Ok(mut socket) = socket else { return };
            let mut request = Vec::new();
            let mut chunk = [0u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                match socket.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(read) => request.extend_from_slice(&chunk[..read]),
                }
            }
            let header_lines: String = headers
                .iter()
                .map(|(name, value)| format!("{name}: {value}\r\n"))
                .collect();
            let head = format!(
                "HTTP/1.1 {status}\r\n{header_lines}Content-Length: {}\r\n\
                 Connection: close\r\n\r\n",
                body.len() + 1000
            );
            let _ = socket.write_all(head.as_bytes());
            let _ = socket.write_all(body.as_bytes());
        }
    });
    base
}

/// `s` parsed as a URL, which a test supplies and knows to be valid.
pub fn url(s: &str) -> Url {
    Url::parse(s).unwrap()
}

/// Dating off: every cleanup pass keeps each bookmark's stored date, and sync writes none.
pub const NO_DATING: DateOpts = DateOpts {
    use_post_date: false,
    max_age_days: 30,
    stale_to_now: false,
};

/// An existing Pinboard bookmark at `url`, with every other field empty.
pub fn stored_at(url: &str) -> Bookmark {
    Bookmark {
        url: Url::parse(url).unwrap(),
        title: String::new(),
        note: String::new(),
        tags: Tags::default(),
        timestamp: None,
        public: false,
        read_later: false,
    }
}

/// `items` as owned strings, for a tag list or any other `Vec<String>` a test spells out.
pub fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|item| item.to_string()).collect()
}

/// [`strings`] as a bookmark's [`Tags`].
pub fn tag_list(items: &[&str]) -> Tags {
    strings(items).into()
}

/// The titles of `drafts`, in order.
pub fn titles(drafts: &[BookmarkDraft]) -> Vec<&str> {
    drafts.iter().map(|d| d.bookmark.title.as_str()).collect()
}

/// Build a `RedditListingEntry` from `kind` (`t3`/`t1`) and a `data` JSON object.
pub fn listing_entry(kind: &str, data: Value) -> RedditListingEntry {
    serde_json::from_value(serde_json::json!({ "kind": kind, "data": data })).unwrap()
}

#[derive(Default)]
pub struct FakeReddit {
    pub saved: Vec<RedditListingEntry>,
    /// What `/api/info` knows; `info` answers with the entries whose fullname was asked for.
    pub info: Vec<RedditListingEntry>,
    /// When set, `info` fails with it instead of answering -- for exercising the
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
    fn dedup_key(&self, url: &Url) -> Option<DedupKey> {
        RedditKeys::new([RedditConfig::default().domain]).dedup_key(url)
    }
}

impl PostInfo for FakeReddit {
    async fn info(&self, fullnames: &[String]) -> Result<Vec<RedditListingEntry>, SourceError> {
        match &self.info_error {
            Some(SourceError::Halted(halt)) => Err(halt.clone().into()),
            Some(SourceError::Other(e)) => Err(SourceError::Other(anyhow::anyhow!("{e}"))),
            None => Ok(self
                .info
                .iter()
                .filter(|entry| {
                    entry
                        .fields
                        .name
                        .as_ref()
                        .is_some_and(|name| fullnames.contains(name))
                })
                .cloned()
                .collect()),
        }
    }
}

/// The recorded `dt` a write would send, formatted by
/// [`crate::timefmt::to_pinboard_dt`], or empty.
fn bookmark_dt(b: &Bookmark) -> String {
    b.timestamp
        .and_then(crate::timefmt::to_pinboard_dt)
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
    /// The account a cleanup store over this fake starts from.
    pub all: Vec<Bookmark>,
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
    /// The successful writes and deletes, interleaved in call order -- see [`replay`].
    pub ops: RefCell<Vec<Op>>,
    /// Trim a note too long for a `posts/add` URL as the Pinboard client does, both in
    /// [`BookmarkStore::fit`] and in what `add` and `update` record.
    pub trims_notes: bool,
    /// The client whose fit `trims_notes` applies, built on first use. Public only so a
    /// test can build the fake with `..Default::default()`.
    pub trimmer: std::cell::OnceCell<PinboardClient>,
}

impl FakePinboard {
    /// The production cleanup store over this fake, its view seeded with `all`.
    pub fn cleanup_store(&self) -> CleanupStore<'_, Self> {
        CleanupStore::new(self, AccountView::new(self.all.clone()), WriteMode::Live)
    }

    /// [`Self::cleanup_store`] for a dry run.
    pub fn dry_cleanup_store(&self) -> CleanupStore<'_, Self> {
        CleanupStore::new(self, AccountView::new(self.all.clone()), WriteMode::DryRun)
    }
}

impl BookmarkStore for FakePinboard {
    fn fit(&self, b: Bookmark) -> Bookmark {
        if !self.trims_notes {
            return b;
        }
        let pinboard = self.trimmer.get_or_init(|| {
            PinboardClient::with_base_url("user:tok".into(), "http://pinboard.invalid".into())
                .unwrap()
        });
        pinboard.fit(b)
    }

    async fn add(&self, b: &Bookmark) -> Result<()> {
        let b = &self.fit(b.clone());
        if self.fail_add_urls.contains(b.url.as_str()) {
            return Err(anyhow!("simulated add failure for {}", b.url));
        }
        self.added.borrow_mut().push(AddCall {
            url: b.url.to_string(),
            tags: b.tags.to_vec(),
            toread: b.read_later,
            shared: b.public,
            dt: bookmark_dt(b),
        });
        self.ops.borrow_mut().push(Op::Write(b.clone()));
        Ok(())
    }
    async fn update(&self, b: &Bookmark) -> Result<()> {
        let b = &self.fit(b.clone());
        if self.fail_update_urls.contains(b.url.as_str()) {
            return Err(anyhow!("simulated update failure for {}", b.url));
        }
        self.updated.borrow_mut().push(UpdateCall {
            url: b.url.to_string(),
            description: b.title.clone(),
            extended: b.note.clone(),
            tags: b.tags.to_vec(),
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

/// The dating policies a sync-then-cleanup round trip is run under: off, on, and on with
/// stale posts re-dated to now. The cap is the default 30 days.
pub const ROUND_TRIP_DATINGS: [DateOpts; 3] = [
    NO_DATING,
    DateOpts {
        use_post_date: true,
        max_age_days: 30,
        stale_to_now: false,
    },
    DateOpts {
        use_post_date: true,
        max_age_days: 30,
        stale_to_now: true,
    },
];

/// The account one `sync` run of `source` leaves on an empty account under `dates`, each
/// bookmark read back as `posts/all` returns it. Written through a [`FakePinboard`] that
/// trims notes, so a note too long for the request URL is stored cut, as the client cuts
/// it; [`assert_cleanup_changes_nothing`] cleans up through one too. Pinboard dates a
/// bookmark sent without a `dt` when it lands, so such a bookmark reads back dated to the
/// run. Not modelled: Pinboard's own limits, such as a long title.
pub async fn synced_account<S: Source>(source: &S, dates: DateOpts) -> Vec<Bookmark> {
    let drafts = crate::http::recording_waits(source.fetch())
        .await
        .0
        .unwrap();
    let now = OffsetDateTime::now_utc().truncate_to_second();
    let settings = JobSettings {
        limit: SyncLimit::All,
        read_later: false,
        public: false,
        dates,
    };
    let new = prepare_new_drafts(source, drafts, &[], &settings, now);
    let pinboard = FakePinboard {
        trims_notes: true,
        ..Default::default()
    };
    let tally = write_drafts(&pinboard, &new, WriteMode::Live).await;
    assert_eq!(tally.failed, 0);
    replay(&[], &pinboard.ops.take())
        .into_iter()
        .map(|written| read_back(written, now))
        .collect()
}

/// `written` as `posts/all` returns it after a `posts/add` that landed at `landed`.
fn read_back(written: Bookmark, landed: OffsetDateTime) -> Bookmark {
    let flag = |set: bool| if set { "yes" } else { "no" }.to_string();
    let wire = PinboardBookmark {
        url: written.url.to_string(),
        description: written.title,
        extended: written.note,
        tags: written.tags.joined(),
        time: crate::timefmt::to_pinboard_dt(written.timestamp.unwrap_or(landed))
            .unwrap_or_default(),
        shared: flag(written.public),
        toread: flag(written.read_later),
    };
    Bookmark::try_from(wire).unwrap()
}

/// Runs `cleanup` over `account` and asserts it changed nothing: no write, merge or delete,
/// and no warning or error. The run forgives a lookup failing in the minority, which would
/// leave that bookmark untouched and the test passing for the wrong reason.
pub async fn assert_cleanup_changes_nothing(
    account: Vec<Bookmark>,
    cleanup: impl AsyncFnOnce(&CleanupStore<'_, FakePinboard>) -> Result<(), SourceError>,
) {
    let pinboard = FakePinboard {
        all: account,
        trims_notes: true,
        ..Default::default()
    };
    let (result, logs) = capturing_logs(cleanup(&pinboard.cleanup_store())).await;
    result.unwrap();
    let complaints: Vec<&String> = logs
        .iter()
        .filter(|line| line.starts_with("WARN") || line.starts_with("ERROR"))
        .collect();
    assert!(complaints.is_empty(), "{complaints:#?}");
    let updated = pinboard.updated.borrow();
    assert!(
        updated.is_empty(),
        "cleanup rewrote what sync wrote: {updated:#?}\nas sync stored it: {:#?}",
        pinboard.all
    );
    assert!(
        pinboard.added.borrow().is_empty(),
        "{:?}",
        pinboard.added.borrow()
    );
    assert!(
        pinboard.deleted.borrow().is_empty(),
        "{:?}",
        pinboard.deleted.borrow()
    );
}

/// Parse `argv` as the binary does, but with no flag falling back to an environment
/// variable: clap reads the process environment at parse time, which a test must not
/// depend on. `cli`'s tests pin which flags the environment backs.
pub fn parse_cli<T: Into<std::ffi::OsString> + Clone>(
    argv: impl IntoIterator<Item = T>,
) -> Result<crate::cli::Cli, clap::Error> {
    use clap::{CommandFactory, FromArgMatches};

    fn without_env(command: clap::Command) -> clap::Command {
        command
            .mut_args(|arg| arg.env(None))
            .mut_subcommands(without_env)
    }
    let matches = without_env(crate::cli::Cli::command()).try_get_matches_from(argv)?;
    crate::cli::Cli::from_arg_matches(&matches)
}
