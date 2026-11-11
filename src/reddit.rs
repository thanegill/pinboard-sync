//! The Reddit clients: [`RedditClient`] reads the user's saved listing
//! (`/user/<name>/saved.json`, for `sync`), and [`RedditInfoClient`] looks posts up in
//! `/api/info.json` (for `cleanup`), reading the saved listing instead while Reddit's edge
//! blocks it. Both send a `reddit_session` cookie over native-tls, no OAuth app: the
//! anti-bot edge refuses cookieless requests and rustls's TLS fingerprint.

pub mod cleanup;
pub mod cookie;
pub mod model;

use std::collections::HashSet;
use std::ops::ControlFlow;
use std::time::Duration;

use anyhow::Context;
use log::{debug, warn};
use serde::de::DeserializeOwned;
use url::Url;

use crate::backup::{BackupDump, BackupSource, ExportBookmark, RawKind, RawSink, PRIMARY_STREAM};
use crate::bookmark::Bookmark;
use crate::config::own_then_configured;
use crate::http::{client_builder, read_body, RetryingClient, DEFAULT_BACKOFF, USER_AGENT};
use crate::keys::RedditKeys;
use crate::reddit::cookie::{refused_cookie_advice, SessionCookie};
use crate::reddit::model::{RedditConfig, RedditListing, RedditListingEntry};
use crate::secrets::ConfiguredSecret;
use crate::source::{
    page_is_caught_up, BookmarkDraft, DedupKey, Halt, Source, SourceError, UrlKey,
};

/// Serves both endpoints, and matches the bookmark URLs.
const REDDIT_BASE: &str = "https://old.reddit.com";

/// Saved items per listing page: the most Reddit returns.
const SAVED_PAGE_SIZE: &str = "100";
/// Fullnames per `/api/info` request: the most Reddit accepts.
const INFO_BATCH: usize = 100;
/// Backstop against an `after` cursor that never nulls out: a listing caps near 1000 items,
/// so this is many full listings. A repeating cursor is the `visited` guard's.
const MAX_SAVED_PAGES: u32 = 100;
/// Reddit's rate-limit pause before each saved page or `/api/info` batch after the first.
const REQUEST_PAUSE: Duration = Duration::from_millis(1000);

/// A Reddit request, as the messages about it name it.
#[derive(Debug, Clone, Copy)]
struct RedditRequest {
    /// What messages call the request.
    what: &'static str,
    /// The pinboard-sync issue tracking Reddit's edge blocking it.
    block_issue: u32,
}

/// The saved listing, for `sync`, `backup` and the `/api/info` fallback.
const SAVED_LISTING: RedditRequest = RedditRequest {
    what: "saved listing",
    block_issue: 78,
};
/// `/api/info`, for `cleanup reddit`.
const API_INFO: RedditRequest = RedditRequest {
    what: "api/info",
    block_issue: 89,
};

/// The reads `cleanup reddit` makes: `/api/info` lookups by fullname, and the saved listing
/// that stands in while Reddit's edge blocks them (#89).
pub trait PostInfo {
    /// Look `fullnames` up. A batch blocked by the edge's network-security page ends the
    /// lookups without an error, answering the batches before it and the block.
    async fn info(&self, fullnames: &[String]) -> Result<InfoAnswer, SourceError>;

    /// Every item in the saved listing, or `None` when no username names whose to read.
    async fn saved(&self) -> Option<Result<Vec<RedditListingEntry>, SourceError>>;
}

/// What `/api/info` answered.
#[derive(Debug, Default)]
pub struct InfoAnswer {
    /// The entries found, from every batch it answered.
    pub entries: Vec<RedditListingEntry>,
    /// The [`Halt::Blocked`] message of the batch that ended the lookups.
    pub blocked: Option<String>,
}

/// What both Reddit clients send over: native-tls, no redirects, and the session cookie.
struct RedditSession {
    http: RetryingClient,
    cookie: Option<SessionCookie>,
    base: String,
}

impl RedditSession {
    fn new(cookie: Option<ConfiguredSecret>, base: String) -> anyhow::Result<Self> {
        // No redirects: one to `/login` means a dead session (see `redirect_error`).
        let http = client_builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http: RetryingClient::new(http, DEFAULT_BACKOFF),
            cookie: cookie.and_then(|cookie| SessionCookie::try_from(cookie).ok()),
            base,
        })
    }

    fn with_cookie(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.cookie {
            Some(cookie) => req.header(reqwest::header::COOKIE, &cookie.header),
            None => req,
        }
    }

    /// `username`'s whole saved listing, newest first, each body pushed to `sink`.
    async fn read_saved(
        &self,
        username: &str,
        sink: &mut RawSink,
    ) -> Result<Vec<RedditListingEntry>, SourceError> {
        let mut out: Vec<RedditListingEntry> = Vec::new();
        self.walk_saved(username, sink, |page| {
            out.extend(page);
            ControlFlow::Continue(())
        })
        .await?;
        Ok(out)
    }

    /// Follow `username`'s saved listing's `after` cursor, handing each page's entries to
    /// `on_page`, until the listing ends, `on_page` breaks (caught up), the cursor loops
    /// back, or [`MAX_SAVED_PAGES`] is reached.
    async fn walk_saved(
        &self,
        username: &str,
        sink: &mut RawSink,
        mut on_page: impl FnMut(Vec<RedditListingEntry>) -> ControlFlow<()>,
    ) -> Result<(), SourceError> {
        let endpoint = format!("{}/user/{username}/saved.json", self.base);
        let mut after: Option<String> = None;
        let mut visited: HashSet<String> = HashSet::new();
        let mut page: u32 = 0;

        loop {
            page += 1;
            if page > MAX_SAVED_PAGES {
                warn!(
                    "reddit saved for '{username}': hit the {MAX_SAVED_PAGES}-page cap; \
                     stopping (some saved items may be missing)"
                );
                sink.mark_truncated(format!(
                    "reddit saved listing stopped at the {MAX_SAVED_PAGES}-page cap"
                ));
                break;
            }
            if page > 1 {
                crate::http::sleep(REQUEST_PAUSE).await;
            }
            let after_ref = after.as_deref();
            let resp = self
                .http
                .send(SAVED_LISTING.what, || {
                    let mut req = self
                        .http
                        .get(&endpoint)
                        .query(&[("limit", SAVED_PAGE_SIZE), ("raw_json", "1")]);
                    if let Some(a) = after_ref {
                        req = req.query(&[("after", a)]);
                    }
                    self.with_cookie(req)
                })
                .await?;
            let body = reddit_body(resp, SAVED_LISTING, self.cookie.as_ref()).await?;
            sink.push(PRIMARY_STREAM, RawKind::Json, &body);
            let listing: RedditListing =
                serde_json::from_str(&body).context("parsing saved listing response")?;
            let got = listing.data.children.len();
            if on_page(listing.data.children).is_break() {
                debug!("reddit saved for '{username}': page {page} has nothing new; stopping");
                break;
            }

            match listing.data.after {
                Some(a) if got > 0 => {
                    if !visited.insert(a.clone()) {
                        warn!(
                            "reddit saved for '{username}': 'after' cursor looped back to \
                             {a}; stopping"
                        );
                        break;
                    }
                    after = Some(a);
                }
                _ => break,
            }
        }

        Ok(())
    }
}

/// Reads a user's saved listing, for `sync` and `backup`.
pub struct RedditClient {
    session: RedditSession,
    /// Whose saved listing to read.
    username: String,
    /// The bookmark domain and tags drafts are shaped with.
    config: RedditConfig,
    /// Every configured account's `reddit_domain`, so a bookmark another account's
    /// `cleanup reddit` moved onto its mirror still keys as this save.
    reddit_domains: Vec<String>,
}

impl RedditClient {
    pub fn new(
        username: String,
        cookie: Option<ConfiguredSecret>,
        config: RedditConfig,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            session: RedditSession::new(cookie, REDDIT_BASE.to_owned())?,
            username,
            config,
            reddit_domains: Vec::new(),
        })
    }

    /// Key bookmarks on any of `domains` as Reddit too, besides this account's own.
    #[must_use]
    pub fn with_reddit_domains(self, domains: Vec<String>) -> Self {
        Self {
            reddit_domains: domains,
            ..self
        }
    }

    /// The whole saved listing, newest first.
    async fn fetch_saved(&self) -> Result<Vec<RedditListingEntry>, SourceError> {
        self.session
            .read_saved(&self.username, &mut RawSink::disabled())
            .await
    }

    /// The drafts `entries` make. A non-empty listing making none is an error: every entry
    /// was dropped (a renamed field reads as `None`), and `sync` would import nothing and
    /// still exit 0.
    fn drafts_from(
        &self,
        entries: Vec<RedditListingEntry>,
    ) -> Result<Vec<BookmarkDraft>, SourceError> {
        let entry_count = entries.len();
        not_all_dropped(entry_count, self.shape(entries))
    }

    /// The drafts `entries` make, skipping (with a warning) any that make none.
    fn shape(&self, entries: Vec<RedditListingEntry>) -> Vec<BookmarkDraft> {
        entries
            .into_iter()
            .filter_map(|e| e.into_saved_item(&self.config.domain))
            .filter_map(|it| it.into_draft(&self.config))
            .collect()
    }

    /// Built per use from `config.domain`, the one place the account's own domain lives.
    fn keys(&self) -> RedditKeys {
        let domains = own_then_configured(&self.config.domain, &self.reddit_domains);
        RedditKeys::new(domains)
    }
}

/// Looks posts up by fullname via `/api/info.json`, for `cleanup reddit`, and reads
/// `username`'s saved listing in its place while Reddit's edge blocks it.
pub struct RedditInfoClient {
    session: RedditSession,
    /// Whose saved listing to read, if a username is configured.
    username: Option<String>,
}

impl RedditInfoClient {
    pub fn new(cookie: ConfiguredSecret, username: Option<String>) -> anyhow::Result<Self> {
        Ok(Self {
            session: RedditSession::new(Some(cookie), REDDIT_BASE.to_owned())?,
            username,
        })
    }
}

impl PostInfo for RedditInfoClient {
    /// The entries that exist, [`INFO_BATCH`] fullnames per request.
    async fn info(&self, fullnames: &[String]) -> Result<InfoAnswer, SourceError> {
        let session = &self.session;
        let mut out: Vec<RedditListingEntry> = Vec::new();
        let endpoint = format!("{}/api/info.json", session.base);
        for (i, chunk) in fullnames.chunks(INFO_BATCH).enumerate() {
            if i > 0 {
                crate::http::sleep(REQUEST_PAUSE).await;
            }
            let ids = chunk.join(",");
            let resp = session
                .http
                .send(API_INFO.what, || {
                    session.with_cookie(
                        session
                            .http
                            .get(&endpoint)
                            .query(&[("id", ids.as_str()), ("raw_json", "1")]),
                    )
                })
                .await?;
            let listing: RedditListing =
                match decode_reddit_json(resp, API_INFO, session.cookie.as_ref()).await {
                    Ok(listing) => listing,
                    Err(SourceError::Halted(Halt::Blocked(block))) => {
                        return Ok(InfoAnswer {
                            entries: out,
                            blocked: Some(block),
                        });
                    }
                    Err(err) => return Err(err),
                };
            out.extend(listing.data.children);
        }
        Ok(InfoAnswer {
            entries: out,
            blocked: None,
        })
    }

    /// The saved listing, read over the `/api/info` session after one [`REQUEST_PAUSE`],
    /// since it follows a lookup.
    async fn saved(&self) -> Option<Result<Vec<RedditListingEntry>, SourceError>> {
        let username = self.username.as_deref()?;
        crate::http::sleep(REQUEST_PAUSE).await;
        Some(
            self.session
                .read_saved(username, &mut RawSink::disabled())
                .await,
        )
    }
}

impl Source for RedditClient {
    /// The saved listing as drafts ([`RedditClient::drafts_from`]).
    async fn fetch(&self) -> Result<Vec<BookmarkDraft>, SourceError> {
        self.drafts_from(self.fetch_saved().await?)
    }

    /// The saved listing is newest-saved first, so paging stops at its first page that
    /// would give `sync` nothing to write.
    async fn fetch_incremental(
        &self,
        existing: &[Bookmark],
    ) -> Result<Vec<BookmarkDraft>, SourceError> {
        let synced_keys = self.synced_keys(existing);
        let mut entry_count = 0;
        let mut drafts = Vec::new();
        self.session
            .walk_saved(&self.username, &mut RawSink::disabled(), |page| {
                let page_len = page.len();
                entry_count += page_len;
                let page_drafts = self.shape(page);
                let caught_up = page_is_caught_up(page_len, &page_drafts, &synced_keys);
                drafts.extend(page_drafts);
                if caught_up {
                    ControlFlow::Break(())
                } else {
                    ControlFlow::Continue(())
                }
            })
            .await?;
        not_all_dropped(entry_count, drafts)
    }
}

/// `drafts`, unless `entry_count` saved entries made none at all: see
/// [`RedditClient::drafts_from`].
fn not_all_dropped(
    entry_count: usize,
    drafts: Vec<BookmarkDraft>,
) -> Result<Vec<BookmarkDraft>, SourceError> {
    if entry_count > 0 && drafts.is_empty() {
        return Err(anyhow::anyhow!(
            "all {entry_count} saved reddit entries were dropped (none yielded a \
             bookmark) — the saved-listing response shape may have changed"
        )
        .into());
    }
    Ok(drafts)
}

impl BackupSource for RedditClient {
    /// The listing is read in full before any is shaped, so a shape break is reported with
    /// the raw pages kept: a schema change is when they are worth having. GitHub and
    /// HackerNews shape as they page, so a failure there leaves a capture too incomplete to
    /// write over a good snapshot.
    async fn dump(&self) -> Result<BackupDump, SourceError> {
        let mut sink = RawSink::collecting();
        let entries = self.session.read_saved(&self.username, &mut sink).await?;
        let records = self
            .drafts_from(entries)
            .map(|drafts| drafts.iter().map(ExportBookmark::from).collect())
            .map_err(SourceError::into_anyhow);
        Ok(sink.into_dump(records))
    }
}

impl UrlKey for RedditClient {
    fn dedup_key(&self, url: &Url) -> Option<DedupKey> {
        self.keys().dedup_key(url)
    }

    fn synced_keys(&self, existing: &[Bookmark]) -> HashSet<DedupKey> {
        self.keys().synced_keys(existing)
    }
}

/// The error for a redirect: [`Halt::Reauth`] for one to `/login`, Reddit asking for a
/// session it accepts; a plain error for any other target.
fn redirect_error(
    resp: &reqwest::Response,
    what: &str,
    cookie_source: Option<&str>,
) -> SourceError {
    let status = resp.status();
    let Some(location) = resp
        .headers()
        .get(reqwest::header::LOCATION)
        .and_then(|value| value.to_str().ok())
    else {
        return anyhow::anyhow!("{what} returned {status} with no Location").into();
    };
    let to_login = resp.url().join(location).is_ok_and(|target| {
        let path = target.path();
        path == "/login" || path.starts_with("/login/")
    });
    if to_login {
        return SourceError::Halted(Halt::Reauth(format!(
            "{what} returned {status}, redirecting to Reddit's login page: Reddit didn't \
             accept the session cookie. {}",
            refused_cookie_advice(cookie_source)
        )));
    }
    anyhow::anyhow!("{what} returned {status}, redirecting to {location:?}").into()
}

/// The body text of a Reddit response, with every status decided here:
/// - the network-security page ([`edge_page`]) is [`Halt::Blocked`], and any other 401/403
///   [`Halt::Reauth`], each with a fixed message rather than the body (logged at debug
///   level when it is neither edge page);
/// - a redirect is not followed but reported by [`redirect_error`];
/// - any other failure status, or a body cut off on a success or 401/403 (a partial 403
///   page can't tell a block from a refused cookie), is [`SourceError::Other`].
///
/// A block's message links `request`'s issue. Messages name the setting `cookie` came from,
/// and its [secrets](SessionCookie::secrets) are redacted from any body shown.
async fn reddit_body(
    resp: reqwest::Response,
    request: RedditRequest,
    cookie: Option<&SessionCookie>,
) -> Result<String, SourceError> {
    use reqwest::header::CONTENT_TYPE;
    use reqwest::StatusCode;

    let RedditRequest { what, block_issue } = request;

    let cookie_source = cookie.map(|cookie| cookie.source);
    let secrets = cookie.map(SessionCookie::secrets).unwrap_or_default();
    let status = resp.status();
    if status.is_redirection() {
        return Err(redirect_error(&resp, what, cookie_source));
    }
    let content_type = resp
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    let refused = status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN;
    let decides = status.is_success() || refused;
    let body = read_body(resp, decides, || {
        if refused {
            format!("reading {what} response ({status})")
        } else {
            format!("reading {what} response")
        }
    })
    .await?;
    if refused {
        let reauth = format!(
            "{what} returned {status}. {}",
            refused_cookie_advice(cookie_source)
        );
        return Err(match edge_page(status, &content_type, &body) {
            Some(EdgePage::NetworkSecurity) => SourceError::Halted(Halt::Blocked(format!(
                "{what} returned {status} — Reddit's edge blocked the request with its \
                 network-security page. Re-copying the cookie will not lift the \
                 block; tracked in https://github.com/thanegill/pinboard-sync/issues/{block_issue}."
            ))),
            Some(EdgePage::NetworkPolicy) => SourceError::Halted(Halt::Reauth(format!(
                "{reauth} (Reddit answered with its \"network policy\" page, which it \
                 serves for an invalid credential; if the cookie is fresh, the host's IP \
                 may be blocked instead.)"
            ))),
            None => {
                warn!(
                    "{what} returned {status} ({} bytes of {content_type:?}), not a \
                     recognised edge page; treating it as an expired cookie",
                    body.len()
                );
                crate::http::log_body_at_debug(
                    &format!("{what} returned {status}"),
                    &body,
                    &secrets,
                );
                SourceError::Halted(Halt::Reauth(reauth))
            }
        });
    }
    if !status.is_success() {
        return Err(crate::http::error_with_body(
            &format!("{what} returned {status}"),
            &body,
            &secrets,
        )
        .into());
    }
    Ok(body)
}

/// One of the HTML pages Reddit's edge answers a refused request with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EdgePage {
    /// "You've been blocked by network security" (#78, and #89 for `/api/info`): served
    /// with a *valid* cookie, so no credential change lifts it.
    NetworkSecurity,
    /// "whoa there, pardner! Your request has been blocked due to a network policy": served
    /// for a junk cookie too, so it stays a credential problem.
    NetworkPolicy,
}

/// Which edge page a response is: a 403 `text/html` body holding its phrase, matched
/// without the apostrophe, whose spelling varies, case-insensitively and with whitespace
/// collapsed, so markup and line breaks don't matter. A real API 403 is JSON, which the
/// content type rules out.
fn edge_page(status: reqwest::StatusCode, content_type: &str, body: &str) -> Option<EdgePage> {
    if status != reqwest::StatusCode::FORBIDDEN
        || !content_type.to_ascii_lowercase().starts_with("text/html")
    {
        return None;
    }
    let body = crate::htmltext::collapse_whitespace(body, " ").to_lowercase();
    if body.contains("blocked by network security") {
        Some(EdgePage::NetworkSecurity)
    } else if body.contains("blocked due to a network policy") {
        Some(EdgePage::NetworkPolicy)
    } else {
        None
    }
}

/// Decode a Reddit JSON response into `T`, over [`reddit_body`]'s status handling.
async fn decode_reddit_json<T: DeserializeOwned>(
    resp: reqwest::Response,
    request: RedditRequest,
    cookie: Option<&SessionCookie>,
) -> Result<T, SourceError> {
    let body = reddit_body(resp, request, cookie).await?;
    Ok(
        serde_json::from_str(&body)
            .with_context(|| format!("parsing {} response", request.what))?,
    )
}

#[cfg(test)]
const TEST_COOKIE_SOURCE: &str = "$REDDIT_COOKIE_FILE";

#[cfg(test)]
impl RedditSession {
    fn with_base_url(cookie: Option<String>, base: String) -> Self {
        let cookie = cookie.map(|value| ConfiguredSecret {
            value,
            source: TEST_COOKIE_SOURCE,
        });
        Self::new(cookie, base).unwrap()
    }
}

#[cfg(test)]
impl RedditClient {
    /// Test constructor reading `username`'s saved listing from a mock server.
    pub(crate) fn with_base_url(cookie: Option<String>, username: &str, base: String) -> Self {
        Self {
            session: RedditSession::with_base_url(cookie, base),
            username: username.into(),
            config: RedditConfig::default(),
            reddit_domains: Vec::new(),
        }
    }
}

#[cfg(test)]
impl RedditInfoClient {
    /// Test constructor looking posts up, and reading `username`'s saved listing, on a mock
    /// server.
    pub(crate) fn with_base_url(
        cookie: Option<String>,
        username: Option<&str>,
        base: String,
    ) -> Self {
        Self {
            session: RedditSession::with_base_url(cookie, base),
            username: username.map(String::from),
        }
    }

    /// Whose saved listing the client reads in place of a blocked `/api/info`.
    pub(crate) fn username(&self) -> Option<&str> {
        self.username.as_deref()
    }
}

#[cfg(test)]
mod tests;

/// Integration tests against a `wiremock` server. These bind a TCP socket, so
/// they can't run in the Nix build sandbox -- the flake skips `net_tests` there.
#[cfg(test)]
mod net_tests;
