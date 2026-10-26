//! The Reddit client: reads the user's private **saved listing**
//! (`/user/<name>/saved.json`, for `sync`) and looks up post metadata via
//! **`/api/info.json`** (for `cleanup`). Both run over the same transport — a
//! `reddit_session` cookie + native-tls, no OAuth app — because Reddit's anti-bot
//! edge refuses cookieless requests *and* rustls's TLS fingerprint (see CLAUDE.md).
//! The cookie authenticates the private saved listing; the username (non-secret)
//! selects whose saves to read.

pub mod cleanup;
pub mod model;

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context;
use log::{debug, warn};
use serde::de::DeserializeOwned;
use url::Url;

use crate::backup::{BackupDump, BackupSource, ExportBookmark, RawKind, RawSink};
use crate::bookmark::Bookmark;
use crate::http::{client_builder, RetryingClient, USER_AGENT};
use crate::reddit::model::{reddit_key, RedditConfig, RedditListing, RedditListingEntry};
use crate::secrets::{reddit_cookie_settings, ConfiguredSecret};
use crate::source::{page_is_caught_up, BookmarkDraft, Source, SourceError, UrlKey};

/// Host serving both the saved listing and `/api/info.json`; old.reddit.com
/// matches the bookmark URLs.
const REDDIT_BASE: &str = "https://old.reddit.com";

/// Retry budget for transient failures on Reddit requests.
const MAX_RETRIES: u32 = 4;
const RETRY_DELAY: Duration = Duration::from_secs(2);
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

/// The `Cookie` header sent for a configured cookie, as [`session_cookie`] shapes it.
struct SessionCookie {
    header: String,
    source: &'static str,
}

impl SessionCookie {
    /// What to [redact](crate::http::redact) from a response body that is logged or
    /// quoted: the whole header and its `reddit_session` value, without the whitespace and
    /// quotes a server would drop before echoing it.
    fn secrets(&self) -> Vec<&str> {
        let session = self
            .header
            .split(';')
            .find_map(session_value)
            .map(|session| session.trim().trim_matches('"'))
            .filter(|session| !session.is_empty());
        [Some(self.header.as_str()), session]
            .into_iter()
            .flatten()
            .collect()
    }
}

impl TryFrom<ConfiguredSecret> for SessionCookie {
    type Error = NoSession;
    fn try_from(cookie: ConfiguredSecret) -> Result<Self, Self::Error> {
        Ok(Self {
            header: session_cookie(&cookie.value, cookie.source).ok_or(NoSession)?,
            source: cookie.source,
        })
    }
}

/// Reads Reddit's saved listing and `/api/info.json`, authenticated by a session
/// cookie over native-tls.
pub struct RedditClient {
    http: RetryingClient,
    cookie: Option<SessionCookie>,
    /// Whose saved listing to read; required by `fetch_saved`.
    username: Option<String>,
    /// Host base for both endpoints (overridden in tests).
    base: String,
    /// Per-account config: bookmark domain + tag vocabulary for shaping drafts.
    config: RedditConfig,
}

impl RedditClient {
    /// Client for `sync`: reads `<base>/user/<username>/saved.json` and shapes
    /// drafts using `config` (bookmark domain + tag vocabulary).
    pub fn for_user(
        username: String,
        cookie: Option<ConfiguredSecret>,
        config: RedditConfig,
    ) -> anyhow::Result<Self> {
        Self::build(cookie, Some(username), REDDIT_BASE.to_string(), config)
    }

    /// Client for `cleanup`: `/api/info.json` lookups only (no saved listing, so
    /// the bookmark domain/tag config are irrelevant).
    pub fn for_info(cookie: Option<ConfiguredSecret>) -> anyhow::Result<Self> {
        Self::build(
            cookie,
            None,
            REDDIT_BASE.to_string(),
            RedditConfig::default(),
        )
    }

    fn build(
        cookie: Option<ConfiguredSecret>,
        username: Option<String>,
        base: String,
        config: RedditConfig,
    ) -> anyhow::Result<Self> {
        // native-tls is the only TLS backend compiled in (see Cargo.toml), so it's
        // the default connector — Reddit's edge rejects rustls's TLS fingerprint, and
        // a ClientHello without ALPN (see `http::client_builder`).
        let http = client_builder()
            .user_agent(USER_AGENT)
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http: RetryingClient::new(http, MAX_RETRIES, RETRY_DELAY),
            cookie: cookie.and_then(|cookie| SessionCookie::try_from(cookie).ok()),
            username,
            base,
            config,
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

impl RedditClient {
    /// Fetch all saved items, newest first, following `after` pagination (Reddit
    /// caps any listing at ~1000). Inherent (no longer a port): only `sync`'s
    /// `Source::fetch` and the net tests call it, both within this crate.
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
            false
        })
        .await?;
        Ok(out)
    }

    /// Follow the saved listing's `after` cursor, handing each page's entries to
    /// `on_page`, until the listing ends, `on_page` returns true (caught up), the cursor
    /// loops back, or [`MAX_SAVED_PAGES`] is reached.
    async fn walk_saved(
        &self,
        sink: &mut RawSink,
        mut on_page: impl FnMut(Vec<RedditListingEntry>) -> bool,
    ) -> Result<(), SourceError> {
        let username = self
            .username
            .as_deref()
            .context("no username configured (set REDDIT_USERNAME)")?;
        let endpoint = format!("{}/user/{}/saved.json", self.base, username);
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
                .send("saved listing", || {
                    let mut req = self
                        .http
                        .get(&endpoint)
                        .query(&[("limit", "100"), ("raw_json", "1")]);
                    if let Some(a) = after_ref {
                        req = req.query(&[("after", a)]);
                    }
                    self.with_cookie(req)
                })
                .await?;
            let body = reddit_body(resp, "saved listing", self.cookie.as_ref()).await?;
            sink.push("", RawKind::Json, &body);
            let listing: RedditListing =
                serde_json::from_str(&body).context("parsing saved listing response")?;
            let got = listing.data.children.len();
            if on_page(listing.data.children) {
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

impl PostInfo for RedditClient {
    /// Batch-fetch Thing data by fullname via `/api/info.json` (100 per request).
    /// Returns the entries that exist.
    async fn info(&self, fullnames: &[String]) -> Result<Vec<RedditListingEntry>, SourceError> {
        let mut out: Vec<RedditListingEntry> = Vec::new();
        let endpoint = format!("{}/api/info.json", self.base);
        for (i, chunk) in fullnames.chunks(100).enumerate() {
            if i > 0 {
                crate::http::sleep(REQUEST_PAUSE).await;
            }
            let ids = chunk.join(",");
            let resp = self
                .http
                .send("api/info", || {
                    self.with_cookie(
                        self.http
                            .get(&endpoint)
                            .query(&[("id", ids.as_str()), ("raw_json", "1")]),
                    )
                })
                .await?;
            let listing: RedditListing =
                decode_reddit_json(resp, "api/info", self.cookie.as_ref()).await?;
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
            caught_up
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
    /// reported *without* discarding the raw pages — a schema change is exactly when the
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
        let (raw, truncated) = sink.into_parts();
        Ok(BackupDump {
            raw,
            records,
            truncated,
        })
    }
}

impl UrlKey for RedditClient {
    /// Reddit's dedup key is the host-agnostic permalink path, so the same post matches
    /// across `old.`/`www.`/`m.` subdomains.
    fn dedup_key(&self, url: &Url) -> Option<String> {
        reddit_key(url)
    }
}

/// The `Cookie` header for a configured cookie. A value that already names a
/// `reddit_session` cookie, alone or in a whole copied `Cookie` header, is sent with only
/// its first non-empty `reddit_session` (see [`with_one_session`]); a bare session value,
/// including one ending in base64 `=` padding, becomes `reddit_session=<value>`, since
/// Reddit only recognises the session by that name. Other cookies without a
/// `reddit_session` are sent with a warning: naming them would only produce a garbled
/// header. Every form goes out [without blank pairs](without_blank_pairs), so a stray `;`
/// doesn't turn a bare value into other cookies. See [`NoSession`] for what counts as no
/// cookie. `source` names the setting in that warning.
fn session_cookie(raw: &str, source: &str) -> Option<String> {
    let value = without_blank_pairs(raw);
    match CookieForm::try_from(value.as_str()).ok()? {
        CookieForm::Named => Some(with_one_session(&value)),
        CookieForm::Bare => Some(format!("reddit_session={value}")),
        CookieForm::OtherCookies => {
            warn!("{}", unnamed_cookie_warning(source));
            Some(value)
        }
    }
}

fn unnamed_cookie_warning(source: &str) -> String {
    format!(
        "the Reddit cookie from {source} doesn't name a reddit_session cookie (expected the \
         bare value or reddit_session=<value>, with no `Cookie:` prefix); sending it as is, \
         and Reddit will likely refuse it"
    )
}

/// What to do about a session Reddit refused, naming the setting the cookie came from, or
/// the settings to use when none was configured.
pub(crate) fn refused_cookie_advice(cookie_source: Option<&str>) -> String {
    match cookie_source {
        Some(source) => format!(
            "The cookie from {source} has likely expired or isn't a reddit_session value; \
             re-copy reddit_session from a logged-in browser."
        ),
        None => format!(
            "No Reddit cookie is configured; set {} to reddit_session from a logged-in \
             browser.",
            reddit_cookie_settings()
        ),
    }
}

/// Whether `raw` configures a Reddit session at all (see [`NoSession`]), so secret
/// resolution can skip one that doesn't as it skips a blank value.
pub fn is_configured_cookie(raw: &str) -> bool {
    CookieForm::try_from(without_blank_pairs(raw).as_str()).is_ok()
}

/// `raw` [`unquoted`], without its [blank](is_blank) cookie pairs, so with no `;` at either
/// end or next to another.
fn without_blank_pairs(raw: &str) -> String {
    unquoted(raw)
        .split(';')
        .filter(|pair| !is_blank(pair))
        .collect::<Vec<_>>()
        .join(";")
        .trim()
        .to_string()
}

/// `raw` trimmed of whitespace and `;`, without one pair of surrounding double quotes, as
/// an env file or shell assignment can leave them. A lone quote at either end is kept: it
/// isn't a pair.
fn unquoted(raw: &str) -> &str {
    let is_edge = |c: char| c == ';' || c.is_whitespace();
    let trimmed = raw.trim_matches(is_edge);
    trimmed
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .map_or(trimmed, |inner| inner.trim_matches(is_edge))
}

/// How a configured cookie value is shaped; see [`session_cookie`].
#[derive(Debug, PartialEq, Eq)]
enum CookieForm {
    /// Names a non-[blank](is_blank) `reddit_session` cookie, as its first pair or after a
    /// `;`.
    Named,
    /// A session value alone: no `;`, and no `=` but trailing base64 padding.
    Bare,
    /// `name=value` pairs, none of them `reddit_session`.
    OtherCookies,
}

/// The cookie value configures no session: [blank](is_blank), or naming `reddit_session`
/// only with blank values.
#[derive(Debug, PartialEq, Eq)]
struct NoSession;

/// The shape of a cookie value, or [`NoSession`]. Expects [`without_blank_pairs`]'s
/// output: a quoted `name=value` reads as other cookies, and so does a bare value with a
/// stray `;`.
impl TryFrom<&str> for CookieForm {
    type Error = NoSession;
    fn try_from(value: &str) -> Result<Self, Self::Error> {
        if is_blank(value) {
            return Err(NoSession);
        }
        let mut sessions = value.split(';').filter_map(session_value).peekable();
        if sessions.peek().is_some() {
            return sessions
                .any(|session| !is_blank(session))
                .then_some(CookieForm::Named)
                .ok_or(NoSession);
        }
        Ok(
            if value.contains(';') || value.trim_end_matches('=').contains('=') {
                CookieForm::OtherCookies
            } else {
                CookieForm::Bare
            },
        )
    }
}

/// Whether a cookie or session value holds nothing but whitespace, double quotes and the
/// `;`/`=` separators, so it has no `name=value` pair a session could be.
fn is_blank(value: &str) -> bool {
    value
        .trim_matches(|c: char| matches!(c, '"' | ';' | '=') || c.is_whitespace())
        .is_empty()
}

/// The value of a `reddit_session=<value>` cookie pair, or `None` for any other pair.
fn session_value(pair: &str) -> Option<&str> {
    pair.trim_start().strip_prefix("reddit_session=")
}

/// `value`'s cookie pairs, in order, without any `reddit_session` but the first non-empty
/// one. Sent several, Reddit would pick one by its own rule, so an empty pair could mask
/// the real session. Expects [`without_blank_pairs`]'s output.
fn with_one_session(value: &str) -> String {
    let mut session_kept = false;
    let pairs: Vec<&str> = value
        .split(';')
        .filter(|pair| match session_value(pair) {
            None => true,
            Some(session) if session_kept || is_blank(session) => false,
            Some(_) => {
                session_kept = true;
                true
            }
        })
        .collect();
    pairs.join(";").trim().to_string()
}

/// The error for a redirect. A redirect to `/login` means Reddit wants a session it
/// accepts (seen for a request with no cookie), so that becomes
/// [`SourceError::ReauthRequired`]; any other target is a plain error.
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
        return SourceError::ReauthRequired(format!(
            "{what} returned {status}, redirecting to Reddit's login page: Reddit didn't \
             accept the session cookie. {}",
            refused_cookie_advice(cookie_source)
        ));
    }
    anyhow::anyhow!("{what} returned {status}, redirecting to {location:?}").into()
}

/// The body text of a Reddit response, centralizing status handling. Reddit's
/// network-security page (see [`edge_page`]) becomes [`SourceError::Blocked`] and any
/// other 401/403 [`SourceError::ReauthRequired`]; both carry a fixed, actionable message,
/// since the body is usually a large HTML page and isn't echoed (one that is neither edge
/// page is logged at debug level instead). Redirects are not followed (see
/// [`redirect_error`]). Any other non-success status becomes [`SourceError::Other`].
/// `what` names the request, and `cookie` is the one it sent: its setting is named in
/// advice and its [secrets](SessionCookie::secrets) are redacted from a logged or quoted
/// body.
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
    let body = resp.text().await.unwrap_or_default();
    if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
        let reauth = format!(
            "{what} returned {status}. {}",
            refused_cookie_advice(cookie_source)
        );
        return Err(match edge_page(status, &content_type, &body) {
            Some(EdgePage::NetworkSecurity) => SourceError::Blocked(format!(
                "{what} returned {status} — Reddit's edge blocked the request with its \
                 network-security page. Re-copying the cookie will not lift the \
                 block; tracked in https://github.com/thanegill/pinboard-sync/issues/78."
            )),
            Some(EdgePage::NetworkPolicy) => SourceError::ReauthRequired(format!(
                "{reauth} (Reddit answered with its \"network policy\" page, which it \
                 serves for an invalid credential; if the cookie is fresh, the host's IP \
                 may be blocked instead.)"
            )),
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
                SourceError::ReauthRequired(reauth)
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
impl RedditClient {
    /// Test constructor pointing both endpoints at a mock server.
    pub(crate) fn for_test(cookie: Option<String>, username: Option<String>, base: String) -> Self {
        let cookie = cookie.map(|value| ConfiguredSecret {
            value,
            source: TEST_COOKIE_SOURCE,
        });
        Self::build(cookie, username, base, RedditConfig::default()).unwrap()
    }
}

#[cfg(test)]
mod tests;

/// Integration tests against a `wiremock` server. These bind a TCP socket, so
/// they can't run in the Nix build sandbox — the flake skips `net_tests` there.
#[cfg(test)]
mod net_tests;
