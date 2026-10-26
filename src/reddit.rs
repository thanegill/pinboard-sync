//! The Reddit client: reads the user's private **saved listing**
//! (`/user/<name>/saved.json`, for `sync`) and looks up post metadata via
//! **`/api/info.json`** (for `cleanup`). Both run over the same transport — a
//! `reddit_session` cookie + native-tls, no OAuth app — because Reddit's anti-bot
//! edge refuses cookieless requests *and* rustls's TLS fingerprint (see CLAUDE.md).
//! The cookie authenticates the private saved listing; the username (non-secret)
//! selects whose saves to read.

use std::collections::HashSet;
use std::time::Duration;

use anyhow::Context;
use log::{debug, warn};
use serde::de::DeserializeOwned;
use url::Url;

use crate::backup::{BackupDump, BackupSource, ExportBookmark, RawKind, RawSink};
use crate::bookmark::Bookmark;
use crate::http::{client_builder, RetryingClient};
use crate::model::{reddit_key, RedditConfig, RedditListing, RedditListingEntry};
use crate::source::{
    page_is_caught_up, reddit_cookie_settings, BookmarkDraft, ConfiguredSecret, Source,
    SourceError, UrlKey,
};

/// Descriptive User-Agent built from the crate name + version. Reddit rejects
/// generic or missing User-Agents.
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

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
#[allow(async_fn_in_trait)]
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
            let listing: RedditListing = serde_json::from_str(&body)
                .context("parsing saved listing response")
                .map_err(SourceError::Other)?;
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
        let chunks: Vec<&[String]> = fullnames.chunks(100).collect();
        let endpoint = format!("{}/api/info.json", self.base);
        for (i, chunk) in chunks.iter().enumerate() {
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
            if i + 1 < chunks.len() {
                crate::http::sleep(REQUEST_PAUSE).await;
            }
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
    serde_json::from_str(&body)
        .with_context(|| format!("parsing {what} response"))
        .map_err(Into::into)
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
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, PartialEq, Deserialize)]
    struct Sample {
        ok: bool,
    }

    fn response(status: u16, body: &str) -> reqwest::Response {
        reqwest::Response::from(
            http::Response::builder()
                .status(status)
                .body(body.to_string())
                .unwrap(),
        )
    }

    fn test_cookie() -> SessionCookie {
        SessionCookie {
            header: "a=1; reddit_session=test-session; token_v2=x".into(),
            source: TEST_COOKIE_SOURCE,
        }
    }

    #[test]
    fn a_cookie_secret_is_the_session_as_a_server_would_echo_it() {
        let quoted = SessionCookie {
            header: "reddit_session=\"abc\" ; a=1".into(),
            source: TEST_COOKIE_SOURCE,
        };
        assert_eq!(quoted.secrets(), [quoted.header.as_str(), "abc"]);
        let unnamed = SessionCookie {
            header: "a=1".into(),
            source: TEST_COOKIE_SOURCE,
        };
        assert_eq!(unnamed.secrets(), ["a=1"]);
    }

    #[tokio::test]
    async fn redacts_the_cookie_from_a_logged_or_quoted_body() {
        let echo = "sent a=1; reddit_session=test-session; token_v2=x, session test-session";
        let redacted = "sent [redacted], session [redacted]";
        let (result, logs) = crate::test_support::capturing_logs(decode_reddit_json::<Sample>(
            response(500, echo),
            "req",
            Some(&test_cookie()),
        ))
        .await;
        let Err(SourceError::Other(err)) = result else {
            panic!("a 500 is a plain error: {result:?}");
        };
        assert_eq!(
            err.to_string(),
            format!("req returned 500 Internal Server Error: {redacted}")
        );
        assert_eq!(
            logs,
            [format!(
                "DEBUG req returned 500 Internal Server Error; full response body: {redacted:?}"
            )]
        );

        let (result, logs) = crate::test_support::capturing_logs(decode_reddit_json::<Sample>(
            html_response(403, echo),
            "req",
            Some(&test_cookie()),
        ))
        .await;
        assert!(matches!(result, Err(SourceError::ReauthRequired(_))));
        let [_, debug] = logs.as_slice() else {
            panic!("expected the size warning and the body, got {logs:?}");
        };
        assert!(debug.ends_with(&format!("{redacted:?}")), "{debug}");
    }

    #[tokio::test]
    async fn maps_401_and_403_to_reauth() {
        for code in [401u16, 403] {
            let err =
                decode_reddit_json::<Sample>(response(code, "denied"), "req", Some(&test_cookie()))
                    .await
                    .unwrap_err();
            assert!(
                matches!(err, SourceError::ReauthRequired(_)),
                "status {code} should require re-auth"
            );
        }
    }

    /// A minimal stand-in for the page Reddit's edge has served JSON clients since
    /// 2026-10-20 (redlib-org/redlib#591), built from its reported text, curly apostrophe
    /// and all; the real one wraps that text in about 190KB of markup.
    pub(super) const BLOCK_PAGE: &str = "<!DOCTYPE html><html><head><title>Blocked</title>\
        </head><body><h1>You\u{2019}ve been blocked by network security.</h1><p>If you \
        think you\u{2019}ve been blocked by mistake, file a ticket below and we\u{2019}ll \
        look into it.</p><a href=\"https://support.reddithelp.com/hc/en-us/requests/new?\
        ticket_form_id=21879292693140\">File a ticket</a></body></html>";

    const HTML: &str = "text/html; charset=utf-8";

    fn html_response(status: u16, body: &str) -> reqwest::Response {
        reqwest::Response::from(
            http::Response::builder()
                .status(status)
                .header("content-type", HTML)
                .body(body.to_string())
                .unwrap(),
        )
    }

    /// A minimal stand-in for the ~1.5KB page Reddit's edge serves for a junk cookie or
    /// bearer token (and for blocked IP ranges).
    const NETWORK_POLICY_PAGE: &str = "<!doctype html><html><head><title>Blocked</title>\
        </head><body><h1>whoa there, pardner!</h1><p>Your request has been blocked due \
        to a network policy.</p><p>Try logging in or creating an account here to get \
        back to browsing.</p></body></html>";

    #[test]
    fn recognises_reddit_edge_pages() {
        let forbidden = reqwest::StatusCode::FORBIDDEN;
        let security = Some(EdgePage::NetworkSecurity);
        assert_eq!(edge_page(forbidden, HTML, BLOCK_PAGE), security);
        assert_eq!(
            edge_page(
                forbidden,
                "TEXT/HTML",
                "YOU'VE BEEN BLOCKED BY NETWORK SECURITY"
            ),
            security
        );
        assert_eq!(
            edge_page(
                forbidden,
                HTML,
                "<h1>You've been blocked by\n      network  security.</h1>"
            ),
            security
        );
        assert_eq!(
            edge_page(forbidden, "text/html", NETWORK_POLICY_PAGE),
            Some(EdgePage::NetworkPolicy)
        );
    }

    #[test]
    fn does_not_mistake_other_refusals_for_an_edge_page() {
        let forbidden = reqwest::StatusCode::FORBIDDEN;
        for body in [
            "",
            "denied",
            "blocked",
            r#"{"message": "Forbidden", "error": 403}"#,
            "<html><title>reddit: log in</title><body>Log in to continue</body></html>",
        ] {
            assert_eq!(
                edge_page(forbidden, HTML, body),
                None,
                "{body:?} is not an edge page"
            );
        }
        assert_eq!(
            edge_page(forbidden, "application/json", BLOCK_PAGE),
            None,
            "a JSON body is an API answer, not the edge's page"
        );
        assert_eq!(
            edge_page(reqwest::StatusCode::UNAUTHORIZED, HTML, BLOCK_PAGE),
            None,
            "only a 403 is an edge page"
        );
    }

    #[tokio::test]
    async fn keeps_the_network_policy_page_a_credential_problem() {
        let err = decode_reddit_json::<Sample>(
            html_response(403, NETWORK_POLICY_PAGE),
            "req",
            Some(&test_cookie()),
        )
        .await
        .unwrap_err();
        let SourceError::ReauthRequired(message) = err else {
            panic!("a junk credential must still fire the hook: {err:?}");
        };
        assert!(message.contains(TEST_COOKIE_SOURCE), "{message}");
        assert!(message.contains("network policy"), "{message}");
    }

    #[tokio::test]
    async fn keeps_other_403s_as_reauth() {
        let json_forbidden = reqwest::Response::from(
            http::Response::builder()
                .status(403)
                .header("content-type", "application/json; charset=UTF-8")
                .body(r#"{"message": "Forbidden", "error": 403}"#.to_string())
                .unwrap(),
        );
        for (label, resp) in [
            ("JSON API 403", json_forbidden),
            (
                "HTML without the phrase",
                html_response(403, "<html><title>reddit: log in</title></html>"),
            ),
            (
                "block text without a content type",
                response(403, BLOCK_PAGE),
            ),
        ] {
            let err = decode_reddit_json::<Sample>(resp, "req", Some(&test_cookie()))
                .await
                .unwrap_err();
            assert!(
                matches!(err, SourceError::ReauthRequired(_)),
                "{label}: got {err:?}"
            );
        }
    }

    #[tokio::test]
    async fn logs_an_unrecognised_refusal_page_at_debug() {
        let page = "<html>\n<title>reddit: log in</title>\n</html>";
        let (result, logs) = crate::test_support::capturing_logs(decode_reddit_json::<Sample>(
            html_response(403, page),
            "req",
            Some(&test_cookie()),
        ))
        .await;
        assert!(matches!(result, Err(SourceError::ReauthRequired(_))));
        let [warning, debug] = logs.as_slice() else {
            panic!("expected the size warning and the body, got {logs:?}");
        };
        assert!(
            warning.starts_with("WARN req returned 403 Forbidden ("),
            "{warning}"
        );
        assert!(
            warning.contains(&format!("{} bytes", page.len())),
            "{warning}"
        );
        assert_eq!(
            debug,
            &format!("DEBUG req returned 403 Forbidden; full response body: {page:?}")
        );
    }

    #[tokio::test]
    async fn maps_a_403_block_page_to_blocked_not_reauth() {
        let err = decode_reddit_json::<Sample>(
            html_response(403, BLOCK_PAGE),
            "req",
            Some(&test_cookie()),
        )
        .await
        .unwrap_err();
        let SourceError::Blocked(message) = err else {
            panic!("a block page must not ask for a new cookie: {err:?}");
        };
        assert!(message.contains("issues/78"), "{message}");
        assert!(!message.contains("likely expired"), "{message}");
    }

    #[tokio::test]
    async fn maps_500_to_other() {
        let err = decode_reddit_json::<Sample>(response(500, "boom"), "req", Some(&test_cookie()))
            .await
            .unwrap_err();
        assert!(matches!(err, SourceError::Other(_)));
    }

    #[tokio::test]
    async fn parses_success_and_reports_bad_json_as_other() {
        let ok = decode_reddit_json::<Sample>(
            response(200, r#"{"ok":true}"#),
            "req",
            Some(&test_cookie()),
        )
        .await
        .unwrap();
        assert_eq!(ok, Sample { ok: true });

        let err =
            decode_reddit_json::<Sample>(response(200, "not json"), "req", Some(&test_cookie()))
                .await
                .unwrap_err();
        assert!(matches!(err, SourceError::Other(_)));
    }

    fn redirect(location: Option<&str>) -> reqwest::Response {
        let mut builder = http::Response::builder().status(302);
        if let Some(location) = location {
            builder = builder.header("location", location);
        }
        reqwest::Response::from(builder.body(String::new()).unwrap())
    }

    #[tokio::test]
    async fn maps_a_redirect_to_login_to_reauth() {
        for location in [
            "https://old.reddit.com/login/?dest=https%3A%2F%2Fold.reddit.com%2Fuser%2Fx%2Fsaved.json",
            "/login",
        ] {
            let err = decode_reddit_json::<Sample>(
                redirect(Some(location)),
                "req",
                Some(&test_cookie()),
            )
            .await
            .unwrap_err();
            let SourceError::ReauthRequired(message) = err else {
                panic!("a login redirect to {location} must ask for a new cookie: {err:?}");
            };
            assert!(message.contains("reddit_session"), "{message}");
        }
    }

    #[tokio::test]
    async fn maps_any_other_redirect_to_other() {
        for location in [
            Some("https://www.reddit.com/user/x/saved.json"),
            Some("/user/login-help"),
            Some("/logins"),
            None,
        ] {
            let err = decode_reddit_json::<Sample>(redirect(location), "req", Some(&test_cookie()))
                .await
                .unwrap_err();
            assert!(
                matches!(err, SourceError::Other(_)),
                "{location:?} is not a login redirect: {err:?}"
            );
        }
    }

    #[test]
    fn every_entry_point_names_a_bare_cookie() {
        let configured = |value: &str| {
            Some(ConfiguredSecret {
                value: value.into(),
                source: TEST_COOKIE_SOURCE,
            })
        };
        let sent = |client: &RedditClient| client.cookie.as_ref().map(|c| c.header.clone());
        let named = Some("reddit_session=bare-value".to_string());
        let sync = RedditClient::for_user(
            "psophis".into(),
            configured(" bare-value "),
            RedditConfig::default(),
        )
        .unwrap();
        assert_eq!(sent(&sync), named);
        let cleanup = RedditClient::for_info(configured("bare-value\n")).unwrap();
        assert_eq!(sent(&cleanup), named);
    }

    #[tokio::test]
    async fn names_the_cookie_setting_in_every_refusal() {
        let source = "the config's `cookie_file`";
        let cookie = SessionCookie {
            source,
            ..test_cookie()
        };
        let refusals = [
            decode_reddit_json::<Sample>(response(403, "denied"), "req", Some(&cookie)),
            decode_reddit_json::<Sample>(redirect(Some("/login")), "req", Some(&cookie)),
        ];
        for refusal in refusals {
            let SourceError::ReauthRequired(message) = refusal.await.unwrap_err() else {
                panic!("a refused cookie must ask for a new one");
            };
            assert!(message.contains(source), "{message}");
            assert!(!message.contains("REDDIT_COOKIE"), "{message}");
        }
        assert!(unnamed_cookie_warning(source).contains(source));
        assert!(!unnamed_cookie_warning(source).contains("REDDIT_COOKIE"));
    }

    #[tokio::test]
    async fn says_no_cookie_is_configured_when_none_was() {
        let refusals = [
            decode_reddit_json::<Sample>(response(403, "denied"), "req", None),
            decode_reddit_json::<Sample>(redirect(Some("/login")), "req", None),
        ];
        for refusal in refusals {
            let SourceError::ReauthRequired(message) = refusal.await.unwrap_err() else {
                panic!("a refusal without a cookie must ask for one");
            };
            assert!(
                message.contains("No Reddit cookie is configured"),
                "{message}"
            );
        }
    }

    #[test]
    fn names_a_bare_session_value_and_keeps_named_cookies() {
        let cases = [
            ("  abc.def-ghi\n", "reddit_session=abc.def-ghi"),
            ("YWJjZA=", "reddit_session=YWJjZA="),
            ("YWJj==", "reddit_session=YWJj=="),
            ("reddit_session=abc\n", "reddit_session=abc"),
            ("reddit_session=YWJj==", "reddit_session=YWJj=="),
            (
                " reddit_session=abc; token_v2=x ",
                "reddit_session=abc; token_v2=x",
            ),
            (" a=1;reddit_session=abc ", "a=1;reddit_session=abc"),
            ("a=1;  reddit_session=abc", "a=1;  reddit_session=abc"),
            ("a=1; b=2", "a=1; b=2"),
            ("\"abc\"", "reddit_session=abc"),
            (" \"reddit_session=abc\"\n", "reddit_session=abc"),
        ];
        for (raw, sent) in cases {
            assert_eq!(
                session_cookie(raw, TEST_COOKIE_SOURCE).as_deref(),
                Some(sent),
                "{raw:?}"
            );
        }
        for unset in [
            " \n",
            "\"\"",
            "reddit_session=",
            " reddit_session= \n",
            "\"reddit_session=\"",
            "a=1; reddit_session=",
            "reddit_session= ; a=1",
        ] {
            assert_eq!(session_cookie(unset, TEST_COOKIE_SOURCE), None, "{unset:?}");
            assert!(!is_configured_cookie(unset), "{unset:?}");
        }
        assert!(is_configured_cookie("abc"));
    }

    #[test]
    fn strips_only_a_matched_quote_pair_and_treats_bare_quotes_as_unset() {
        let cases = [
            ("\"abc", "reddit_session=\"abc"),
            ("abc\"", "reddit_session=abc\""),
            ("\"\"abc\"\"", "reddit_session=\"abc\""),
            (
                "reddit_session=\"; reddit_session=abc",
                "reddit_session=abc",
            ),
        ];
        for (raw, sent) in cases {
            assert_eq!(
                session_cookie(raw, TEST_COOKIE_SOURCE).as_deref(),
                Some(sent),
                "{raw:?}"
            );
        }
        for unset in [
            "\"",
            " \" \n",
            "\"\"\"",
            "\"\"\"\"",
            "\" \"",
            "reddit_session=\"",
            "reddit_session=\"\"",
            "a=1; reddit_session=\" \"",
        ] {
            assert_eq!(session_cookie(unset, TEST_COOKIE_SOURCE), None, "{unset:?}");
            assert!(!is_configured_cookie(unset), "{unset:?}");
        }
    }

    #[test]
    fn a_cookie_of_only_separators_is_no_cookie() {
        for unset in [
            ";",
            "=",
            "=;",
            " ; = ;\n",
            "\"=\"",
            "reddit_session==",
            "reddit_session=;=",
            "a=1; reddit_session= =",
        ] {
            assert_eq!(session_cookie(unset, TEST_COOKIE_SOURCE), None, "{unset:?}");
            assert!(!is_configured_cookie(unset), "{unset:?}");
        }
        let cases = [
            ("reddit_session==; reddit_session=abc", "reddit_session=abc"),
            ("reddit_session=abc; =; ;", "reddit_session=abc"),
            ("abc=", "reddit_session=abc="),
            ("abc;", "reddit_session=abc"),
            (" ;abc; \n", "reddit_session=abc"),
            ("YWJj==;", "reddit_session=YWJj=="),
            ("\"abc;\"", "reddit_session=abc"),
            ("\"abc\";", "reddit_session=abc"),
            (";;reddit_session=x;", "reddit_session=x"),
            ("a=1;;", "a=1"),
            ("a=1; ;b=2;", "a=1;b=2"),
        ];
        for (raw, sent) in cases {
            assert_eq!(
                session_cookie(raw, TEST_COOKIE_SOURCE).as_deref(),
                Some(sent),
                "{raw:?}"
            );
        }
    }

    #[test]
    fn sends_only_the_first_non_empty_session_whatever_the_order() {
        let cases = [
            ("reddit_session=; reddit_session=abc", "reddit_session=abc"),
            ("reddit_session=abc; reddit_session=", "reddit_session=abc"),
            (
                "reddit_session=abc;reddit_session=def",
                "reddit_session=abc",
            ),
            (
                "a=1; reddit_session= ; reddit_session=abc; b=2; reddit_session=def",
                "a=1; reddit_session=abc; b=2",
            ),
            ("reddit_session=abc;", "reddit_session=abc"),
            ("a=1;; reddit_session=abc", "a=1; reddit_session=abc"),
        ];
        for (raw, sent) in cases {
            assert!(is_configured_cookie(raw), "{raw:?}");
            assert_eq!(
                session_cookie(raw, TEST_COOKIE_SOURCE).as_deref(),
                Some(sent),
                "{raw:?}"
            );
        }
        assert!(!is_configured_cookie("reddit_session=; reddit_session= "));
    }

    #[test]
    fn names_a_session_cookie_only_by_its_name() {
        let cases = [
            ("abc.def-ghi", CookieForm::Bare),
            ("YWJjZA=", CookieForm::Bare),
            ("YWJj==", CookieForm::Bare),
            ("reddit_session=abc", CookieForm::Named),
            ("reddit_session=abc; token_v2=x", CookieForm::Named),
            ("a=1; reddit_session=abc", CookieForm::Named),
            ("a=1;reddit_session=abc", CookieForm::Named),
            ("a=reddit_session=1", CookieForm::OtherCookies),
            ("a=1; b=x reddit_session=2", CookieForm::OtherCookies),
            ("a=1; b=2", CookieForm::OtherCookies),
            ("token_v2=x", CookieForm::OtherCookies),
        ];
        for (trimmed, form) in cases {
            assert_eq!(CookieForm::try_from(trimmed), Ok(form), "{trimmed:?}");
        }
        assert_eq!(CookieForm::try_from(""), Err(NoSession));
    }

    #[test]
    fn a_saved_page_is_caught_up_when_its_saves_are_on_pinboard() {
        use crate::test_support::{listing_entry, stored_at};
        let post = |id: &str| {
            listing_entry(
                "t3",
                serde_json::json!({ "name": format!("t3_{id}"), "subreddit": "rust",
                                    "permalink": format!("/r/rust/comments/{id}/x/"),
                                    "title": "T" }),
            )
        };
        let unshapable = listing_entry("t3", serde_json::json!({ "name": "t3_gone" }));
        let client = RedditClient::for_test(None, None, "http://unused.invalid".into());
        let synced_keys = client.synced_keys(&[
            stored_at("https://www.reddit.com/r/rust/comments/a/x/"),
            stored_at("https://old.reddit.com/r/Rust/comments/b/x/"),
        ]);
        let caught_up = |page: Vec<RedditListingEntry>| {
            page_is_caught_up(page.len(), &client.shape(page), &synced_keys)
        };

        assert!(
            caught_up(vec![post("a"), post("b")]),
            "saves already on Pinboard, under any reddit host or case"
        );
        assert!(!caught_up(vec![post("a"), post("c")]), "a new save");
        assert!(
            caught_up(vec![post("a"), unshapable]),
            "an entry that makes no draft adds nothing"
        );
    }
}

/// Integration tests against a `wiremock` server. These bind a TCP socket, so
/// they can't run in the Nix build sandbox — the flake skips `net_tests` there.
#[cfg(test)]
mod net_tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param, query_param_is_missing};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn fetch_saved_sends_cookie_paginates_and_parses_metadata() {
        let server = MockServer::start().await;
        // Page 1 (no `after`) returns a cursor and a richly-tagged post.
        Mock::given(method("GET"))
            .and(path("/user/psophis/saved.json"))
            .and(header("cookie", "reddit_session=secret"))
            .and(query_param_is_missing("after"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "kind": "Listing",
                "data": { "after": "t3_first", "children": [
                    { "kind": "t3", "data": { "name": "t3_first", "subreddit": "rust",
                      "permalink": "/r/rust/comments/1/x/", "title": "1", "author": "alice",
                      "post_hint": "image" } }
                ] }
            })))
            .mount(&server)
            .await;
        // Page 2 (after=t3_first) is the last.
        Mock::given(method("GET"))
            .and(path("/user/psophis/saved.json"))
            .and(query_param("after", "t3_first"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "kind": "Listing",
                "data": { "after": null, "children": [
                    { "kind": "t1", "data": { "name": "t1_second", "subreddit": "news",
                      "permalink": "/r/news/comments/2/y/b/", "link_title": "T", "body": "hi" } }
                ] }
            })))
            .mount(&server)
            .await;

        let client = RedditClient::for_test(
            Some("reddit_session=secret".into()),
            Some("psophis".into()),
            server.uri(),
        );
        let items: Vec<_> = crate::http::recording_waits(client.fetch_saved())
            .await
            .0
            .unwrap()
            .into_iter()
            .filter_map(|e| e.into_saved_item("old.reddit.com"))
            .collect();
        assert_eq!(items.len(), 2);
        assert_eq!(
            items[0].tags(&RedditConfig::default()),
            vec![
                "reddit",
                "subreddit:rust",
                "author:reddit:alice",
                "type:image"
            ]
        );
        assert!(items[1].is_comment);
    }

    #[tokio::test]
    async fn sends_a_bare_cookie_value_as_reddit_session() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/psophis/saved.json"))
            .and(header("cookie", "reddit_session=bare-value"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "kind": "Listing",
                "data": { "after": null, "children": [] }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = RedditClient::for_test(
            Some("bare-value\n".into()),
            Some("psophis".into()),
            server.uri(),
        );
        assert!(client.fetch_saved().await.unwrap().is_empty());
    }

    /// Reddit's answer to a request without a session it accepts, as `curl` saw it for a
    /// request with no cookie.
    fn login_redirect(server: &MockServer) -> ResponseTemplate {
        ResponseTemplate::new(302).insert_header(
            "location",
            format!(
                "{}/login/?dest=%2Fuser%2Fpsophis%2Fsaved.json",
                server.uri()
            )
            .as_str(),
        )
    }

    /// A login page that answers 403, as a followed redirect can reach; the client must
    /// never request it.
    async fn mount_login_page(server: &MockServer) {
        Mock::given(path("/login/"))
            .respond_with(ResponseTemplate::new(403))
            .expect(0)
            .mount(server)
            .await;
    }

    /// Following the redirect would also end in `ReauthRequired` (the login page's 403),
    /// so the message is what tells the two apart.
    fn assert_refused_at_login(err: SourceError) {
        let SourceError::ReauthRequired(message) = &err else {
            panic!("a login redirect must ask for a new cookie: {err:?}");
        };
        assert!(message.contains("login page"), "{message}");
        assert!(message.contains(TEST_COOKIE_SOURCE), "{message}");
        assert!(!message.contains("stale"), "{message}");
    }

    #[tokio::test]
    async fn fetch_saved_maps_a_login_redirect_to_reauth_without_following_it() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/psophis/saved.json"))
            .respond_with(login_redirect(&server))
            .expect(1)
            .mount(&server)
            .await;
        mount_login_page(&server).await;

        let client = RedditClient::for_test(
            Some("reddit_session=stale".into()),
            Some("psophis".into()),
            server.uri(),
        );
        let err = client.fetch_saved().await.unwrap_err();
        assert_refused_at_login(err);
    }

    #[tokio::test]
    async fn info_maps_a_login_redirect_to_reauth_without_following_it() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/info.json"))
            .respond_with(login_redirect(&server))
            .expect(1)
            .mount(&server)
            .await;
        mount_login_page(&server).await;

        let client =
            RedditClient::for_test(Some("reddit_session=stale".into()), None, server.uri());
        let err = client.info(&["t3_a".to_string()]).await.unwrap_err();
        assert_refused_at_login(err);
    }

    #[tokio::test]
    async fn fetch_saved_terminates_when_after_cursor_loops() {
        // A pathological listing that keeps returning the same non-null `after` with a
        // non-empty page would loop forever without the visited-cursor guard. It must
        // stop after re-seeing the cursor rather than hanging / growing `out` unbounded.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/psophis/saved.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "kind": "Listing",
                "data": { "after": "t3_loop", "children": [
                    { "kind": "t3", "data": { "name": "t3_loop", "subreddit": "rust",
                      "permalink": "/r/rust/comments/1/x/", "title": "1" } }
                ] }
            })))
            .mount(&server)
            .await;

        let client = RedditClient::for_test(None, Some("psophis".into()), server.uri());
        // Page 1 (no cursor) records the cursor; page 2 (after=t3_loop) re-sees it and
        // breaks, so exactly two pages are fetched.
        let (entries, waits) = crate::http::recording_waits(client.fetch_saved()).await;
        assert_eq!(entries.unwrap().len(), 2);
        assert_eq!(waits, [REQUEST_PAUSE]);
    }

    #[tokio::test]
    async fn fetch_errors_when_a_non_empty_listing_yields_no_drafts() {
        // Every entry deserializes (all fields are optional) but lacks the required
        // subreddit/permalink, so all are dropped. A non-empty listing that produces
        // zero drafts must error rather than silently import nothing.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/psophis/saved.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "kind": "Listing",
                "data": { "after": null, "children": [
                    { "kind": "t3", "data": { "name": "t3_a", "title": "A" } },
                    { "kind": "t1", "data": { "name": "t1_b", "link_title": "B" } }
                ] }
            })))
            .mount(&server)
            .await;

        let client = RedditClient::for_test(None, Some("psophis".into()), server.uri());
        assert!(matches!(client.fetch().await, Err(SourceError::Other(_))));
    }

    #[tokio::test]
    async fn fetch_keeps_a_good_entry_and_drops_a_schema_broken_sibling() {
        // One entry is well-formed, the other is missing subreddit/permalink. The good
        // one survives (and the bad one is warned + dropped), so fetch succeeds.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/psophis/saved.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "kind": "Listing",
                "data": { "after": null, "children": [
                    { "kind": "t3", "data": { "name": "t3_a", "subreddit": "rust",
                      "permalink": "/r/rust/comments/a/x/", "title": "A" } },
                    { "kind": "t3", "data": { "name": "t3_b", "title": "B" } }
                ] }
            })))
            .mount(&server)
            .await;

        let client = RedditClient::for_test(None, Some("psophis".into()), server.uri());
        let drafts = client.fetch().await.unwrap();
        assert_eq!(drafts.len(), 1);
        assert_eq!(
            drafts[0].bookmark.url.as_str(),
            "https://old.reddit.com/r/rust/comments/a/x/"
        );
    }

    #[tokio::test]
    async fn fetch_empty_listing_is_ok() {
        // A genuinely empty saved listing (nothing saved) is a success, not an error.
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/psophis/saved.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "kind": "Listing",
                "data": { "after": null, "children": [] }
            })))
            .mount(&server)
            .await;

        let client = RedditClient::for_test(None, Some("psophis".into()), server.uri());
        assert!(client.fetch().await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn fetch_saved_maps_403_to_reauth() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).set_body_string("blocked"))
            .mount(&server)
            .await;
        let client = RedditClient::for_test(None, Some("psophis".into()), server.uri());
        assert!(matches!(
            client.fetch_saved().await,
            Err(SourceError::ReauthRequired(_))
        ));
    }

    #[tokio::test]
    async fn info_names_the_cookie_setting_in_a_403_but_never_the_cookie() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(403).set_body_string("denied"))
            .mount(&server)
            .await;
        let client =
            RedditClient::for_test(Some("reddit_session=SECRET403".into()), None, server.uri());
        let err = client.info(&["t3_a".to_string()]).await.unwrap_err();
        let SourceError::ReauthRequired(message) = &err else {
            panic!("a 403 must ask for a new cookie: {err:?}");
        };
        assert!(message.contains(TEST_COOKIE_SOURCE), "{message}");
        assert!(!message.contains("SECRET403"), "{message}");
    }

    /// The block as Reddit serves it, headers included.
    fn block_page() -> ResponseTemplate {
        ResponseTemplate::new(403)
            .insert_header("server", "snooserv")
            .insert_header("retry-after", "0")
            .set_body_raw(super::tests::BLOCK_PAGE, "text/html; charset=utf-8")
    }

    #[tokio::test]
    async fn fetch_saved_maps_a_block_page_to_blocked() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/psophis/saved.json"))
            .respond_with(block_page())
            .expect(1)
            .mount(&server)
            .await;
        let client = RedditClient::for_test(
            Some("reddit_session=secret".into()),
            Some("psophis".into()),
            server.uri(),
        );
        let err = client.fetch_saved().await.unwrap_err();
        assert!(matches!(err, SourceError::Blocked(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn info_maps_a_block_page_to_blocked() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/info.json"))
            .respond_with(block_page())
            .expect(1)
            .mount(&server)
            .await;
        let client =
            RedditClient::for_test(Some("reddit_session=secret".into()), None, server.uri());
        let err = client.info(&["t3_a".to_string()]).await.unwrap_err();
        assert!(matches!(err, SourceError::Blocked(_)), "got {err:?}");
    }

    #[tokio::test]
    async fn info_sends_a_bare_cookie_value_as_reddit_session() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/info.json"))
            .and(header("cookie", "reddit_session=bare-value"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "kind": "Listing",
                "data": { "after": null, "children": [] }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client = RedditClient::for_test(Some("bare-value".into()), None, server.uri());
        assert!(client.info(&["t3_a".to_string()]).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn info_sends_cookie_and_returns_entries() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/info.json"))
            .and(query_param("id", "t3_a"))
            .and(header("cookie", "reddit_session=secret"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "kind": "Listing",
                "data": { "after": null, "children": [
                    { "kind": "t3", "data": { "name": "t3_a", "subreddit": "rust",
                      "permalink": "/r/rust/comments/a/x/", "title": "A", "over_18": true } }
                ] }
            })))
            .expect(1)
            .mount(&server)
            .await;

        let client =
            RedditClient::for_test(Some("reddit_session=secret".into()), None, server.uri());
        let entries = client.info(&["t3_a".to_string()]).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].fields.over_18);
    }

    /// Mounts a three-page saved listing, newest first, one post per page (`n`, `o`,
    /// then `p`), expecting each page `hits` times.
    async fn three_saved_pages(server: &MockServer, hits: [u64; 3]) {
        let pages = [
            (None, "n", Some("t3_n")),
            (Some("t3_n"), "o", Some("t3_o")),
            (Some("t3_o"), "p", None),
        ];
        for ((after, id, next), hits) in pages.into_iter().zip(hits) {
            let saved = Mock::given(method("GET")).and(path("/user/psophis/saved.json"));
            let saved = match after {
                None => saved.and(query_param_is_missing("after")),
                Some(after) => saved.and(query_param("after", after)),
            };
            saved
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "kind": "Listing",
                    "data": { "after": next, "children": [
                        { "kind": "t3", "data": { "name": format!("t3_{id}"), "subreddit": "rust",
                          "permalink": format!("/r/rust/comments/{id}/x/"), "title": id } }
                    ] }
                })))
                .expect(hits)
                .mount(server)
                .await;
        }
    }

    fn saved_client(server: &MockServer) -> RedditClient {
        RedditClient::for_test(None, Some("psophis".into()), server.uri())
    }

    /// Pinboard already holds the second page's save, so that page has nothing new.
    fn second_page_synced() -> [Bookmark; 1] {
        [crate::test_support::stored_at(
            "https://www.reddit.com/r/rust/comments/o/x/",
        )]
    }

    fn titles(drafts: &[BookmarkDraft]) -> Vec<&str> {
        drafts.iter().map(|d| d.bookmark.title.as_str()).collect()
    }

    #[tokio::test]
    async fn incremental_fetch_stops_at_the_first_page_with_nothing_new() {
        let server = MockServer::start().await;
        three_saved_pages(&server, [1, 1, 0]).await;

        let client = saved_client(&server);
        let (drafts, waits) =
            crate::http::recording_waits(client.fetch_incremental(&second_page_synced())).await;
        assert_eq!(titles(&drafts.unwrap()), ["n", "o"]);
        assert_eq!(waits, [REQUEST_PAUSE], "one pause, between the two pages");
    }

    #[tokio::test]
    async fn incremental_fetch_reads_on_while_every_page_has_something_new() {
        let server = MockServer::start().await;
        three_saved_pages(&server, [1, 1, 1]).await;

        let client = saved_client(&server);
        let (drafts, waits) = crate::http::recording_waits(client.fetch_incremental(&[])).await;
        assert_eq!(titles(&drafts.unwrap()), ["n", "o", "p"]);
        assert_eq!(
            waits, [REQUEST_PAUSE; 2],
            "a pause between pages, none after"
        );
    }

    #[tokio::test]
    async fn the_page_cap_stops_without_a_pause_after_the_last_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/psophis/saved.json"))
            .respond_with(|request: &wiremock::Request| {
                let page = request
                    .url
                    .query_pairs()
                    .find(|(key, _)| key == "after")
                    .map_or(0, |(_, after)| after[3..].parse::<u32>().unwrap() + 1);
                ResponseTemplate::new(200).set_body_json(json!({
                    "kind": "Listing",
                    "data": { "after": format!("t3_{page}"), "children": [
                        { "kind": "t3", "data": { "name": format!("t3_{page}"),
                          "subreddit": "rust", "title": "t",
                          "permalink": format!("/r/rust/comments/{page}/x/") } }
                    ] }
                }))
            })
            .expect(u64::from(MAX_SAVED_PAGES))
            .mount(&server)
            .await;

        let (entries, waits) =
            crate::http::recording_waits(saved_client(&server).fetch_saved()).await;
        assert_eq!(entries.unwrap().len(), MAX_SAVED_PAGES as usize);
        assert_eq!(waits, vec![REQUEST_PAUSE; MAX_SAVED_PAGES as usize - 1]);
    }

    #[tokio::test]
    async fn dump_reads_every_saved_page() {
        let server = MockServer::start().await;
        three_saved_pages(&server, [1, 1, 1]).await;

        let client = saved_client(&server);
        let dump = crate::http::recording_waits(client.dump()).await.0.unwrap();
        assert_eq!(dump.raw.len(), 3);
    }

    #[tokio::test]
    async fn saved_listing_error_quotes_a_capped_one_line_excerpt() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/user/psophis/saved.json"))
            .respond_with(
                ResponseTemplate::new(404).set_body_string(crate::test_support::long_error_page()),
            )
            .mount(&server)
            .await;

        let err = saved_client(&server).fetch_saved().await.unwrap_err();
        crate::test_support::assert_quotes_capped_excerpt(
            &err.to_string(),
            "saved listing returned 404 Not Found",
        );
    }
}
