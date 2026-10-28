//! The Reddit clients: [`RedditClient`] reads the user's private **saved listing**
//! (`/user/<name>/saved.json`, for `sync`), and [`RedditInfoClient`] looks up post
//! metadata via **`/api/info.json`** (for `cleanup`). Both run over the same transport --
//! a `reddit_session` cookie + native-tls, no OAuth app -- because Reddit's anti-bot edge
//! refuses cookieless requests *and* rustls's TLS fingerprint (see CLAUDE.md). The cookie
//! authenticates the saved listing, and the non-secret username picks whose saves to read.

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

/// Host serving both the saved listing and `/api/info.json`; old.reddit.com
/// matches the bookmark URLs.
const REDDIT_BASE: &str = "https://old.reddit.com";

/// Saved items per listing page: the most Reddit returns.
const SAVED_PAGE_SIZE: &str = "100";
/// Fullnames per `/api/info` request: the most Reddit accepts.
const INFO_BATCH: usize = 100;
/// Backstop against a malformed `after` cursor that never nulls out. Reddit caps any
/// listing near 1000 items at 100 per page, so this covers a full saved listing many
/// times over -- it only bounds a response that keeps returning a non-null cursor; the
/// cursor-repeat case is caught by the `visited` guard.
const MAX_SAVED_PAGES: u32 = 100;
/// The pause before each saved page or `/api/info` batch after the first, to be gentle
/// with Reddit's rate limit.
const REQUEST_PAUSE: Duration = Duration::from_millis(1000);

/// Reddit `/api/info` lookups by fullname (used by `cleanup` for over_18/title).
pub trait PostInfo {
    async fn info(&self, fullnames: &[String]) -> Result<Vec<RedditListingEntry>, SourceError>;
}

/// What both Reddit clients send over: native-tls, no redirects, and the session cookie.
struct RedditSession {
    http: RetryingClient,
    cookie: Option<SessionCookie>,
    /// Host base for every endpoint (overridden in tests).
    base: String,
}

impl RedditSession {
    fn new(cookie: Option<ConfiguredSecret>, base: String) -> anyhow::Result<Self> {
        // native-tls is the only TLS backend compiled in (see Cargo.toml), so it's
        // the default connector -- Reddit's edge rejects rustls's TLS fingerprint, and
        // a ClientHello without ALPN (see `http::client_builder`).
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

    /// Attach the session cookie to a request, if one is configured.
    fn with_cookie(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match &self.cookie {
            Some(cookie) => req.header(reqwest::header::COOKIE, &cookie.header),
            None => req,
        }
    }
}

/// Reads a user's saved listing, for `sync` and `backup`.
pub struct RedditClient {
    session: RedditSession,
    /// Whose saved listing to read.
    username: String,
    /// Per-account config: bookmark domain + tag vocabulary for shaping drafts.
    config: RedditConfig,
    /// Every configured account's `reddit_domain`, so a bookmark another account's
    /// `cleanup reddit` moved onto its mirror still keys as this save.
    reddit_domains: Vec<String>,
}

impl RedditClient {
    /// Reads `<base>/user/<username>/saved.json` and shapes drafts using `config`
    /// (bookmark domain + tag vocabulary).
    pub fn new(
        username: String,
        cookie: Option<ConfiguredSecret>,
        config: RedditConfig,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            session: RedditSession::new(cookie, REDDIT_BASE.to_string())?,
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
}

/// Looks posts up by fullname via `/api/info.json`, for `cleanup reddit`.
pub struct RedditInfoClient {
    session: RedditSession,
}

impl RedditInfoClient {
    pub fn new(cookie: ConfiguredSecret) -> anyhow::Result<Self> {
        Ok(Self {
            session: RedditSession::new(Some(cookie), REDDIT_BASE.to_string())?,
        })
    }
}

impl RedditClient {
    /// Fetch all saved items, newest first, following `after` pagination (Reddit caps any
    /// listing at ~1000).
    async fn fetch_saved(&self) -> Result<Vec<RedditListingEntry>, SourceError> {
        self.fetch_saved_capturing(&mut RawSink::disabled()).await
    }

    /// [`fetch_saved`](Self::fetch_saved), pushing each response body to `sink` as it
    /// arrives so `backup` gets the verbatim listing out of the same traversal.
    async fn fetch_saved_capturing(
        &self,
        sink: &mut RawSink,
    ) -> Result<Vec<RedditListingEntry>, SourceError> {
        let mut out: Vec<RedditListingEntry> = Vec::new();
        self.walk_saved(sink, |page| {
            out.extend(page);
            ControlFlow::Continue(())
        })
        .await?;
        Ok(out)
    }

    /// Follow the saved listing's `after` cursor, handing each page's entries to
    /// `on_page`, until the listing ends, `on_page` breaks (caught up), the cursor loops
    /// back, or [`MAX_SAVED_PAGES`] is reached.
    async fn walk_saved(
        &self,
        sink: &mut RawSink,
        mut on_page: impl FnMut(Vec<RedditListingEntry>) -> ControlFlow<()>,
    ) -> Result<(), SourceError> {
        let username = &self.username;
        let session = &self.session;
        let endpoint = format!("{}/user/{username}/saved.json", session.base);
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
            let resp = session
                .http
                .send("saved listing", || {
                    let mut req = session
                        .http
                        .get(&endpoint)
                        .query(&[("limit", SAVED_PAGE_SIZE), ("raw_json", "1")]);
                    if let Some(a) = after_ref {
                        req = req.query(&[("after", a)]);
                    }
                    session.with_cookie(req)
                })
                .await?;
            let body = reddit_body(resp, "saved listing", session.cookie.as_ref()).await?;
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

impl PostInfo for RedditInfoClient {
    /// Batch-fetch Thing data by fullname via `/api/info.json`, [`INFO_BATCH`] per
    /// request. Returns the entries that exist.
    async fn info(&self, fullnames: &[String]) -> Result<Vec<RedditListingEntry>, SourceError> {
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
                .send("api/info", || {
                    session.with_cookie(
                        session
                            .http
                            .get(&endpoint)
                            .query(&[("id", ids.as_str()), ("raw_json", "1")]),
                    )
                })
                .await?;
            let listing: RedditListing =
                decode_reddit_json(resp, "api/info", session.cookie.as_ref()).await?;
            out.extend(listing.data.children);
        }
        Ok(out)
    }
}

impl Source for RedditClient {
    /// Fetch the saved listing and shape each post/comment into a draft. A non-empty
    /// listing that yields zero drafts is an error, not an empty success: it means every
    /// entry was dropped (e.g. a renamed required subfield that still deserializes as
    /// `None`), which would otherwise make `sync` exit 0 having imported nothing.
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
        self.walk_saved(&mut RawSink::disabled(), |page| {
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

impl RedditClient {
    /// Shape saved entries into drafts. A non-empty listing that yields zero drafts is an
    /// error, not an empty success: it means every entry was dropped (e.g. a renamed
    /// required subfield that still deserializes as `None`), which would otherwise make
    /// `sync` exit 0 having imported nothing.
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
    /// The listing is fetched in full before any of it is shaped, so a shape break is
    /// reported *without* discarding the raw pages -- a schema change is exactly when the
    /// verbatim body is worth having. (GitHub and HackerNews shape inline as they
    /// paginate, so a failure there means the capture is incomplete and must not be
    /// written over a good snapshot.)
    async fn dump(&self) -> Result<BackupDump, SourceError> {
        let mut sink = RawSink::collecting();
        let entries = self.fetch_saved_capturing(&mut sink).await?;
        let records = self
            .drafts_from(entries)
            .map(|drafts| drafts.iter().map(ExportBookmark::from).collect())
            .map_err(SourceError::into_anyhow);
        Ok(sink.into_dump(records))
    }
}

impl UrlKey for RedditClient {
    /// Reddit's dedup key is the host-agnostic permalink path, so the same post matches
    /// across `old.`/`www.`/`m.` subdomains and every configured `reddit_domain`.
    fn dedup_key(&self, url: &Url) -> Option<DedupKey> {
        self.keys().dedup_key(url)
    }

    fn synced_keys(&self, existing: &[Bookmark]) -> HashSet<DedupKey> {
        self.keys().synced_keys(existing)
    }
}

impl RedditClient {
    /// Built per use from `config.domain`, the one place the account's own domain lives.
    fn keys(&self) -> RedditKeys {
        let domains = own_then_configured(&self.config.domain, &self.reddit_domains);
        RedditKeys::new(domains)
    }
}

/// The error for a redirect. A redirect to `/login` means Reddit wants a session it
/// accepts (seen for a request with no cookie), so that becomes [`Halt::Reauth`]; any
/// other target is a plain error.
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

/// The body text of a Reddit response, centralizing status handling. Reddit's
/// network-security page (see [`edge_page`]) becomes [`Halt::Blocked`] and any other
/// 401/403 [`Halt::Reauth`]; both carry a fixed, actionable message, since the body is
/// usually a large HTML page and isn't echoed (one that is neither edge page is logged at
/// debug level instead). Redirects are not followed (see [`redirect_error`]). Any other
/// non-success status becomes [`SourceError::Other`], as does a body cut off mid-read on a
/// success or 401/403, since a partial 403 page can't tell the block from a refused
/// cookie. `what` names the request, and `cookie` is the one it sent: its setting is named
/// in advice and its [secrets](SessionCookie::secrets) are redacted from a logged or
/// quoted body.
async fn reddit_body(
    resp: reqwest::Response,
    what: &str,
    cookie: Option<&SessionCookie>,
) -> Result<String, SourceError> {
    use reqwest::header::CONTENT_TYPE;
    use reqwest::StatusCode;

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
        .to_string();
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
                 block; tracked in https://github.com/thanegill/pinboard-sync/issues/78."
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
    /// "You've been blocked by network security" (since 2026-10-20, redlib-org/redlib#591,
    /// thanegill/pinboard-sync#78): served to a request carrying a *valid* cookie, so no
    /// credential change lifts it.
    NetworkSecurity,
    /// "whoa there, pardner! Your request has been blocked due to a network policy":
    /// served for a junk cookie or bearer token as well as for blocked IP ranges, so it
    /// stays a credential problem.
    NetworkPolicy,
}

/// Which edge page a response is, if any: a 403 `text/html` body carrying the page's
/// phrase. Neither phrase includes the apostrophe, and each is matched
/// case-insensitively anywhere in the body with its whitespace collapsed, so the
/// apostrophe's spelling and the markup and line breaks around it don't matter. Reddit's
/// API answers a real 403 with short JSON, which the content type rules out even if it
/// ever quoted a phrase.
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
    what: &str,
    cookie: Option<&SessionCookie>,
) -> Result<T, SourceError> {
    let body = reddit_body(resp, what, cookie).await?;
    Ok(serde_json::from_str(&body).with_context(|| format!("parsing {what} response"))?)
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
    /// Test constructor looking posts up on a mock server.
    pub(crate) fn with_base_url(cookie: Option<String>, base: String) -> Self {
        Self {
            session: RedditSession::with_base_url(cookie, base),
        }
    }
}

#[cfg(test)]
mod tests;

/// Integration tests against a `wiremock` server. These bind a TCP socket, so
/// they can't run in the Nix build sandbox -- the flake skips `net_tests` there.
#[cfg(test)]
mod net_tests;
