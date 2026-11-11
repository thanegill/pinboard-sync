//! In-memory fakes of the ports ([`Source`], [`PostInfo`], [`BookmarkStore`]), so the loops
//! run without a network, plus scratch directories, log and stdout capture, and the
//! sync-then-cleanup round trip.

use std::cell::RefCell;
use std::collections::HashSet;
use std::future::Future;
use std::sync::Once;

use anyhow::{anyhow, Result};
use serde_json::Value;
use tempfile::TempDir;
use time::OffsetDateTime;
use url::Url;

use crate::bookmark::{
    AccountView, Bookmark, BookmarkStore, CleanupStore, TooLongToSend, Update, WriteMode,
};
use crate::config::SyncLimit;
use crate::keys::RedditKeys;
use crate::pinboard::{PinboardBookmark, PinboardClient, PINBOARD_URL_LIMIT};
use crate::reddit::model::{RedditConfig, RedditListingEntry};
use crate::reddit::{InfoAnswer, PostInfo};
use crate::source::{BookmarkDraft, DedupKey, Halt, Source, SourceError, UrlKey};
use crate::sync::{prepare_new_drafts, write_drafts, JobSettings};
use crate::tags::Tags;
use crate::timefmt::DateOpts;

tokio::task_local! {
    static CAPTURED_LOGS: RefCell<Vec<String>>;
    static CAPTURED_STDOUT: RefCell<String>;
}

/// Runs `fut`, returning its output and everything it wrote through [`crate::stdout`].
pub async fn capturing_stdout<T>(fut: impl Future<Output = T>) -> (T, String) {
    CAPTURED_STDOUT
        .scope(RefCell::default(), async {
            let output = fut.await;
            (output, CAPTURED_STDOUT.with(RefCell::take))
        })
        .await
}

/// Append `text` to what the enclosing [`capturing_stdout`] collects; `false` outside one.
pub fn capture_stdout(text: &str) -> bool {
    CAPTURED_STDOUT
        .try_with(|out| out.borrow_mut().push_str(text))
        .is_ok()
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
    at_levels(logs, &["WARN"])
}

/// The warnings and errors among lines [`capturing_logs`] returned.
pub fn complaints(logs: &[String]) -> Vec<&str> {
    at_levels(logs, &["WARN", "ERROR"])
}

/// The lines among `logs` logged at one of `levels`.
fn at_levels<'l>(logs: &'l [String], levels: &[&str]) -> Vec<&'l str> {
    logs.iter()
        .map(String::as_str)
        .filter(|line| levels.iter().any(|level| line.starts_with(level)))
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

/// The base URL of a local HTTP/1.1 server whose every response is cut off: `status`,
/// `content_type` and `body` under a longer `Content-Length`, then a close. It binds a
/// socket, so only a `net_tests` case may call it.
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

/// The read timeout of a test client that must time out quickly.
pub const SHORT_READ_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

/// The config every property test runs under: 64 cases unless `PROPTEST_CASES` sets
/// another count, and no regression files, which proptest would otherwise write to
/// `proptest-regressions/` on a failure.
pub fn proptest_config() -> proptest::test_runner::Config {
    proptest::test_runner::contextualize_config(proptest::test_runner::Config {
        cases: 64,
        failure_persistence: None,
        ..proptest::test_runner::Config::default()
    })
}

/// An existing Pinboard bookmark at `url`, with every other field empty.
pub fn stored_at(url: &str) -> Bookmark {
    Bookmark {
        url: self::url(url),
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
    let mut wire = serde_json::json!({ "kind": kind });
    wire["data"] = data;
    serde_json::from_value(wire).unwrap()
}

#[derive(Default)]
pub struct FakeReddit {
    pub saved: Vec<RedditListingEntry>,
    /// What `/api/info` knows; `info` returns the entries for the fullnames it is asked.
    pub info: Vec<RedditListingEntry>,
    /// When set, `info` fails with it, as an expired cookie that must reach the hook. A
    /// [`Halt::Blocked`] is answered as a block after the entries, as on a later batch.
    pub info_error: Option<SourceError>,
    /// When set, reading the saved listing fails with it.
    pub saved_error: Option<SourceError>,
    /// No username is configured, so the saved listing can't be read.
    pub no_username: bool,
}

/// A copy of `error`, which isn't `Clone`.
fn copy_of(error: &SourceError) -> SourceError {
    match error {
        SourceError::Halted(halt) => halt.clone().into(),
        SourceError::Other(e) => SourceError::Other(anyhow::anyhow!("{e}")),
    }
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

    /// The full fetch: the fake has no pages to stop at.
    async fn fetch_incremental(&self, _: &[Bookmark]) -> Result<Vec<BookmarkDraft>, SourceError> {
        self.fetch().await
    }
}

impl UrlKey for FakeReddit {
    fn dedup_key(&self, url: &Url) -> Option<DedupKey> {
        RedditKeys::new([RedditConfig::default().domain]).dedup_key(url)
    }
}

impl PostInfo for FakeReddit {
    async fn info(&self, fullnames: &[String]) -> Result<InfoAnswer, SourceError> {
        let blocked = match &self.info_error {
            Some(SourceError::Halted(Halt::Blocked(block))) => Some(block.clone()),
            Some(error) => return Err(copy_of(error)),
            None => None,
        };
        Ok(InfoAnswer {
            entries: self
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
                .collect(),
            blocked,
        })
    }

    async fn saved(&self) -> Option<Result<Vec<RedditListingEntry>, SourceError>> {
        if self.no_username {
            return None;
        }
        Some(match &self.saved_error {
            Some(error) => Err(copy_of(error)),
            None => Ok(self.saved.clone()),
        })
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

/// A call that changed the account, to rebuild it from; unlike [`FakePinboard::deleted`],
/// never a failed delete.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    Write(Bookmark),
    Delete(Url),
}

/// The account rebuilt from `seed` and the ops that landed. A plain `Vec`, not an
/// [`crate::bookmark::AccountView`], whose bug would show on both sides of the comparison.
/// Assumes `seed` holds each URL once.
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
    /// Each `add` that succeeded, as sent.
    pub added: RefCell<Vec<AddCall>>,
    /// Each `update` that succeeded, as sent.
    pub updated: RefCell<Vec<UpdateCall>>,
    /// Every URL `delete` was asked to remove, including a delete that then failed.
    pub deleted: RefCell<Vec<String>>,
    /// URLs whose `add` should fail, to exercise log-and-skip behavior.
    pub fail_add_urls: HashSet<String>,
    /// URLs whose `update` should fail, to exercise cleanup log-and-skip behavior.
    pub fail_update_urls: HashSet<String>,
    /// URLs whose `delete` should fail: the only way to a half-applied merge, written but
    /// with an absorbed URL left.
    pub fail_delete_urls: HashSet<String>,
    /// URLs whose `update` the store refuses as too long, as Pinboard answers a 414:
    /// nothing is written.
    pub too_long_urls: HashSet<String>,
    /// URLs too long to send at all, even with no note: [`BookmarkStore::can_write`] says
    /// so up front and `update` refuses them, as the client does.
    pub unwritable_urls: HashSet<String>,
    /// The successful writes and deletes, interleaved in call order -- see [`replay`].
    pub ops: RefCell<Vec<Op>>,
    /// Trim a note too long for a `posts/add` URL as the Pinboard client does, both in
    /// [`BookmarkStore::fit`] and in what `add` and `update` record.
    pub trims_notes: bool,
    /// The client `fit` and `kept_of_cut` delegate to when `trims_notes` is set, built on
    /// first use. Public only so a test can build the fake with `..Default::default()`.
    pub trimmer: std::cell::OnceCell<PinboardClient>,
}

impl FakePinboard {
    /// An empty fake that trims notes as the Pinboard client does (`trims_notes`).
    pub fn trimming() -> Self {
        FakePinboard {
            trims_notes: true,
            ..Default::default()
        }
    }

    /// The URL of each `add` that succeeded, in call order.
    pub fn added_urls(&self) -> Vec<String> {
        self.added
            .borrow()
            .iter()
            .map(|add| add.url.clone())
            .collect()
    }

    /// The URL of each `update` that succeeded, in call order.
    pub fn updated_urls(&self) -> Vec<String> {
        self.updated
            .borrow()
            .iter()
            .map(|update| update.url.clone())
            .collect()
    }

    /// The production cleanup store over this fake, its view seeded with `all`.
    pub fn cleanup_store(&self) -> CleanupStore<'_, Self> {
        CleanupStore::new(self, AccountView::new(self.all.clone()), WriteMode::Live)
    }

    /// `b` as the Pinboard client sends it: fitted, and each tag once.
    fn as_sent(&self, b: &Bookmark) -> Bookmark {
        let fitted = self.fit(b.clone());
        Bookmark {
            tags: Tags::parse(&fitted.tags.joined()),
            ..fitted
        }
    }

    /// [`Self::cleanup_store`] for a dry run.
    pub fn dry_cleanup_store(&self) -> CleanupStore<'_, Self> {
        CleanupStore::new(self, AccountView::new(self.all.clone()), WriteMode::DryRun)
    }

    fn trimming_client(&self) -> &PinboardClient {
        self.trimmer.get_or_init(|| {
            PinboardClient::with_base_url("user:tok".into(), "http://pinboard.invalid".into())
                .unwrap()
        })
    }
}

impl BookmarkStore for FakePinboard {
    fn fit(&self, b: Bookmark) -> Bookmark {
        if !self.trims_notes {
            return b;
        }
        self.trimming_client().fit(b)
    }

    fn kept_of_cut<'n>(&self, note: &'n str) -> Option<&'n str> {
        self.trims_notes
            .then(|| self.trimming_client().kept_of_cut(note))
            .flatten()
    }

    /// A URL in `unwritable_urls` reads as one byte over the client's limit.
    fn can_write(&self, b: &Bookmark) -> Result<(), TooLongToSend> {
        if self.unwritable_urls.contains(b.url.as_str()) {
            return Err(TooLongToSend {
                without_note: PINBOARD_URL_LIMIT + 1,
                limit: PINBOARD_URL_LIMIT,
            });
        }
        if !self.trims_notes {
            return Ok(());
        }
        self.trimming_client().can_write(b)
    }

    async fn add(&self, b: &Bookmark) -> Result<()> {
        let b = &self.as_sent(b);
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
    async fn update(&self, b: &Bookmark) -> Result<Update> {
        let b = &self.as_sent(b);
        if self.fail_update_urls.contains(b.url.as_str()) {
            return Err(anyhow!("simulated update failure for {}", b.url));
        }
        if self.too_long_urls.contains(b.url.as_str()) || self.can_write(b).is_err() {
            return Ok(Update::TooLong);
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
        Ok(Update::Written)
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
        ..NO_DATING
    },
    DateOpts {
        use_post_date: true,
        stale_to_now: true,
        ..NO_DATING
    },
];

/// The account one `sync` of `source` leaves under `dates`, read back as `posts/all`
/// returns it: written through a note-trimming [`FakePinboard`], as is the cleanup in
/// [`assert_cleanup_changes_nothing`], and an undated write dated to the run, as Pinboard
/// does. Pinboard's own limits, such as a long title, aren't modelled.
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
    let pinboard = FakePinboard::trimming();
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

/// Run `cleanup` over `account` and assert it changed nothing and warned of nothing: the
/// run forgives a minority of failed lookups, which would pass for the wrong reason.
pub async fn assert_cleanup_changes_nothing(
    account: Vec<Bookmark>,
    cleanup: impl AsyncFnOnce(&CleanupStore<'_, FakePinboard>) -> Result<(), SourceError>,
) {
    let complained = cleanup_writes_nothing(account, cleanup).await;
    assert!(complained.is_empty(), "{complained:#?}");
}

/// [`assert_cleanup_changes_nothing`] short of the check on what it logs: returns the
/// warnings and errors instead.
pub async fn cleanup_writes_nothing(
    account: Vec<Bookmark>,
    cleanup: impl AsyncFnOnce(&CleanupStore<'_, FakePinboard>) -> Result<(), SourceError>,
) -> Vec<String> {
    let pinboard = FakePinboard {
        all: account,
        ..FakePinboard::trimming()
    };
    let (result, logs) = capturing_logs(cleanup(&pinboard.cleanup_store())).await;
    result.unwrap();
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
    complaints(&logs).into_iter().map(String::from).collect()
}

/// Parse `argv` as the binary does, but with no `env` fallback: clap reads the process
/// environment, which a test must not depend on.
pub fn parse_cli<T: Into<std::ffi::OsString> + Clone>(
    argv: impl IntoIterator<Item = T>,
) -> Result<crate::cli::Cli, clap::Error> {
    use clap::CommandFactory;

    fn without_env(command: clap::Command) -> clap::Command {
        command
            .mut_args(|arg| arg.env(None))
            .mut_subcommands(without_env)
    }
    crate::cli::parse_with(without_env(crate::cli::Cli::command()), argv)
}
