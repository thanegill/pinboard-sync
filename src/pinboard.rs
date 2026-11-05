//! Minimal Pinboard API client: `posts/add`, `posts/all`, and `posts/delete`,
//! with rate limiting and transient-failure retries (see [`crate::http`]).

use std::cell::Cell;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use log::{debug, warn};
use serde::Deserialize;
use url::Url;

use crate::backup::{BackupDump, BackupSource, ExportBookmark, RawKind, RawPage, PRIMARY_STREAM};
use crate::bookmark::{warn_note_cut, Bookmark, BookmarkStore, Update};
use crate::http::{
    answered_a_retry, client_builder, error_with_body, redact, LinearBackoff, RetryingClient,
    REDACTED, USER_AGENT,
};
use crate::source::SourceError;
use crate::tags::Tags;

const PINBOARD_BASE: &str = "https://api.pinboard.in/v1";

/// The wait between `posts/add` calls, a 414's retry included (Pinboard asks for ~3s).
pub const RATE_LIMIT: Duration = Duration::from_secs(3);
/// 5 tries, each retry waiting at least the ~3s Pinboard asks for between calls.
const BACKOFF: LinearBackoff = LinearBackoff::new(5, RATE_LIMIT);

/// `posts/add` is a GET, so every field -- notably `extended` (the notes) -- rides in
/// the request URL, and `extended` is trimmed to keep the whole URL, scheme and host
/// included, percent-encoded as sent, within this many bytes, when the other fields leave
/// room under it (else see [`PinboardClient::fit_extended`]). Other fields are never
/// trimmed. The margin under [`PINBOARD_URL_LIMIT`] absorbs a small change on Pinboard's
/// side; a 414 that still comes back is [`OnTooLong`]'s call.
const MAX_URL_BYTES: usize = 8100;
/// The longest `posts/add` URL Pinboard takes, in encoded bytes, scheme and host included:
/// its nginx caps the request line, and a URL of 8201 bytes passed while one of 8202 got
/// `414 URI Too Long` (measured 2026-11-04).
const PINBOARD_URL_LIMIT: usize = 8201;
/// The 414 retry never tries a budget below this: it stops there and surfaces the 414.
const MIN_URL_BYTES: usize = 500;
/// Appended to a trimmed `extended` so the truncation is visible in the bookmark.
pub(crate) const TRUNCATION_MARKER: &str = "… [truncated]";

/// What `posts/add` does when Pinboard answers `414 URI Too Long` anyway.
#[derive(Clone, Copy)]
enum OnTooLong {
    /// Halve the budget and retry ([`halved`]): a new bookmark has nothing stored to
    /// lose, so a shorter note beats none.
    Halve,
    /// Write nothing: rewriting a stored bookmark with a note shorter than `fit` keeps
    /// would cut text the store had accepted, and a merge judged against the longer note
    /// would then delete what it absorbed.
    Refuse,
}

/// The budget the 414 retry tries after `budget`, or `None` when half of it would be
/// below [`MIN_URL_BYTES`].
fn halved(budget: usize) -> Option<usize> {
    let half = budget / 2;
    (half >= MIN_URL_BYTES).then_some(half)
}

/// What `post_add` does when it gives up the write to `url` without sending it (again),
/// for the reason `why`.
fn unfittable(url: &Url, why: &str, too_long: OnTooLong) -> Result<Update> {
    match too_long {
        OnTooLong::Refuse => {
            debug!("{url}: {why}; nothing written");
            Ok(Update::TooLong)
        }
        OnTooLong::Halve => Err(anyhow!("{why}")),
    }
}

/// A `dt` of the length every `dt` has, for measuring a write that sends none.
const DT_PLACEHOLDER: &str = "1970-01-01T00:00:00Z";

#[derive(Deserialize)]
struct AddResponse {
    result_code: String,
}

impl AddResponse {
    /// `Ok` when Pinboard reports the call `done`, else an error quoting its result code
    /// as `what` failing, through [`redacted_error`].
    fn into_result(self, what: &str, auth_token: &str) -> Result<()> {
        if self.result_code == "done" {
            return Ok(());
        }
        Err(redacted_error(what, &self.result_code, auth_token))
    }
}

/// A bookmark exactly as `posts/all` returns it -- the Pinboard wire shape, with its
/// space-joined tag string, ISO-8601 `time`, and `"yes"/"no"` flags. Converted to the
/// service-agnostic [`Bookmark`] on read (see the `TryFrom` impl below); nothing outside
/// this module handles the wire form.
#[derive(Debug, Clone, Deserialize)]
pub struct PinboardBookmark {
    #[serde(rename = "href")]
    pub url: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub extended: String,
    /// Space-separated tag string.
    #[serde(default)]
    pub tags: String,
    /// Creation timestamp (ISO 8601).
    #[serde(default)]
    pub time: String,
    #[serde(default)]
    pub shared: String,
    #[serde(default)]
    pub toread: String,
}

/// A wire bookmark whose `href` doesn't parse as a URL.
#[derive(Debug, thiserror::Error)]
#[error("unparseable URL {href}: {source}")]
pub struct UnparseableHref {
    href: String,
    source: url::ParseError,
}

impl TryFrom<PinboardBookmark> for Bookmark {
    /// Fails only when the wire `href` doesn't parse as a URL; `all()` skips (and warns
    /// on) such entries. A non-empty `time` that won't parse is *not* fatal: it becomes
    /// `timestamp: None` (logged) so the bookmark stays in the set sync dedups against --
    /// dropping it would make sync re-add the URL and reset its date/title/tags/notes.
    type Error = UnparseableHref;
    fn try_from(b: PinboardBookmark) -> Result<Self, Self::Error> {
        let url = match Url::parse(&b.url) {
            Ok(url) => url,
            Err(source) => {
                return Err(UnparseableHref {
                    href: b.url,
                    source,
                })
            }
        };
        let timestamp = if b.time.is_empty() {
            None
        } else {
            let parsed = crate::timefmt::parse_rfc3339(&b.time);
            if parsed.is_none() {
                warn!(
                    "bookmark {url}: unparseable creation time {:?}, dropping date",
                    b.time
                );
            }
            parsed
        };
        Ok(Bookmark {
            url,
            title: b.description,
            note: b.extended,
            tags: Tags::parse(&b.tags),
            timestamp,
            public: b.shared == "yes",
            read_later: b.toread == "yes",
        })
    }
}

pub struct PinboardClient {
    http: RetryingClient,
    /// `username:TOKEN`.
    auth_token: String,
    /// The pause between successive `posts/add` writes.
    rate_limit: Duration,
    /// Set once the first write has happened, so [`Self::pace`] spaces only *successive*
    /// writes. A `Cell` is enough: the futures aren't `Send`, so it stays on one thread.
    wrote: Cell<bool>,
    /// API base, e.g. `https://api.pinboard.in/v1`.
    base: String,
}

impl PinboardClient {
    pub fn new(auth_token: String, rate_limit: Duration) -> Result<Self> {
        Self::build(
            auth_token,
            rate_limit,
            PINBOARD_BASE.to_string(),
            client_builder(),
        )
    }

    fn build(
        auth_token: String,
        rate_limit: Duration,
        base: String,
        http: reqwest::ClientBuilder,
    ) -> Result<Self> {
        let http = http
            .user_agent(USER_AGENT)
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http: RetryingClient::new(http, BACKOFF),
            auth_token,
            rate_limit,
            wrote: Cell::new(false),
            base,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base)
    }

    /// Pinboard asks for ~3s between `posts/add` calls, so pause before each one after the
    /// first, a 414's retry included: a failed attempt still hit the API, so it counts.
    async fn pace(&self) {
        if self.wrote.get() {
            crate::http::sleep(self.rate_limit).await;
        }
        self.wrote.set(true);
    }

    /// Every bookmark in the account (`posts/all`).
    pub async fn all(&self) -> Result<Vec<Bookmark>> {
        let resp = self.get_posts_all("no").await?;
        let body = resp.text().await.context("reading Pinboard posts/all")?;
        let wire: Vec<PinboardBookmark> =
            serde_json::from_str(&body).context("parsing Pinboard posts/all")?;
        Ok(wire_to_bookmarks(wire))
    }

    /// `posts/add` with `replace=yes`, mapping a domain [`Bookmark`] to the Pinboard
    /// parameters (`public`/`read_later` -> `shared`/`toread`, `timestamp` -> `dt`). A set
    /// `timestamp` sets the bookmark time. `extended` is trimmed if needed to keep the GET
    /// URL under [`MAX_URL_BYTES`], or [`PINBOARD_URL_LIMIT`] when the other fields leave
    /// it no room under the budget; what a 414 does is `too_long`'s call. Nothing is sent
    /// when the URL is over the limit even with no note ([`unfittable`]).
    async fn post_add(&self, b: &Bookmark, too_long: OnTooLong) -> Result<Update> {
        let tags = b.tags.joined();
        let dt = b.timestamp.and_then(crate::timefmt::to_pinboard_dt);
        let fixed = self.add_params(b, &tags, dt.as_deref());
        let sent_without_note = self.add_url_len(&fixed, "");
        if sent_without_note > PINBOARD_URL_LIMIT {
            let why = format!(
                "the posts/add URL without the note is {sent_without_note} bytes, over \
                 Pinboard's {PINBOARD_URL_LIMIT}-byte limit, so cutting the note can't fit it"
            );
            return unfittable(&b.url, &why, too_long);
        }
        let endpoint = self.url("posts/add");
        let measured_without_note = self.measured(b, |fixed| self.add_url_len(fixed, ""));

        // Transient errors (network/429/5xx) are handled by `RetryingClient::send`.
        let mut budget = MAX_URL_BYTES;
        loop {
            self.pace().await;
            let extended = self.fitted_note(b, budget);
            let mut params = fixed.clone();
            params.push(("extended", extended.as_str()));

            let resp = self
                .http
                .send(&endpoint, || self.http.get(&endpoint).query(&params))
                .await?;

            if resp.status() == reqwest::StatusCode::URI_TOO_LONG {
                match (too_long, halved(budget)) {
                    (OnTooLong::Refuse, _) => {
                        debug!("posts/add for {} was too long; nothing written", b.url);
                        return Ok(Update::TooLong);
                    }
                    (OnTooLong::Halve, Some(half)) if measured_without_note > half => {
                        let why = format!(
                            "the posts/add fields other than the note measure \
                             {measured_without_note} bytes, over the next {half}-byte budget, \
                             so a smaller budget can't shorten the URL"
                        );
                        return unfittable(&b.url, &why, too_long).with_context(|| {
                            format!("Pinboard answered 414 at a {budget}-byte budget")
                        });
                    }
                    (OnTooLong::Halve, Some(half)) => {
                        budget = half;
                        continue;
                    }
                    (OnTooLong::Halve, None) => {}
                }
            }
            if !resp.status().is_success() {
                return Err(self.status_error(&endpoint, resp).await);
            }
            let parsed: AddResponse = resp
                .json()
                .await
                .context("parsing Pinboard posts/add response")?;
            parsed.into_result("Pinboard posts/add failed", &self.auth_token)?;
            // Warn once the trim is final, not inside the retry: a note Pinboard keeps
            // rejecting is re-trimmed at each halved budget, and warning per attempt reads
            // like several different notes were cut. Worth saying at all because the stored
            // note is now not the one the caller wrote. `cleanup` hands over notes `fit`
            // already cut and says so itself, so this never fires for them.
            if extended != b.note {
                warn_note_cut(&b.url, b.note.len(), extended.len());
            }
            return Ok(Update::Written);
        }
    }

    /// Every `posts/add` param except `extended`: these are never trimmed, and set the
    /// budget `extended` has to fit within. `tags` and `dt` are `b`'s, rendered for the
    /// wire; with no `dt` the param is left out.
    fn add_params<'a>(
        &'a self,
        b: &'a Bookmark,
        tags: &'a str,
        dt: Option<&'a str>,
    ) -> Vec<(&'a str, &'a str)> {
        let mut fixed = vec![
            ("url", b.url.as_str()),
            ("description", b.title.as_str()),
            ("tags", tags),
            ("replace", "yes"),
            ("shared", if b.public { "yes" } else { "no" }),
            ("toread", if b.read_later { "yes" } else { "no" }),
            ("auth_token", self.auth_token.as_str()),
            ("format", "json"),
        ];
        if let Some(dt) = dt {
            fixed.push(("dt", dt));
        }
        fixed
    }

    /// `b`'s notes as a `posts/add` within `budget` stores them ([`Self::fit_extended`]).
    fn fitted_note(&self, b: &Bookmark, budget: usize) -> String {
        self.measured(b, |fixed| self.fit_extended(fixed, &b.note, budget))
    }

    /// `measure` applied to `b`'s [`Self::add_params`] as the budget counts them: with a
    /// `dt` even when `b` has none. Every `dt` is the same length, so the trim doesn't
    /// depend on whether the write dates the bookmark. Without that, a note `sync` trimmed
    /// undated would trim differently once `cleanup` writes it back with the date Pinboard
    /// gave it, and be rewritten for that alone.
    fn measured<T>(&self, b: &Bookmark, measure: impl FnOnce(&[(&str, &str)]) -> T) -> T {
        let tags = b.tags.joined();
        let dt = b
            .timestamp
            .and_then(crate::timefmt::to_pinboard_dt)
            .unwrap_or_else(|| DT_PLACEHOLDER.to_string());
        measure(&self.add_params(b, &tags, Some(&dt)))
    }

    /// Byte length of the `posts/add` URL reqwest builds from `fixed` and `extended`.
    fn add_url_len(&self, fixed: &[(&str, &str)], extended: &str) -> usize {
        let mut params = fixed.to_vec();
        params.push(("extended", extended));
        self.http
            .get(self.url("posts/add"))
            .query(&params)
            .build()
            .map_or(usize::MAX, |r| r.url().as_str().len())
    }

    /// Trim `extended` so the encoded `posts/add` URL stays within `budget` bytes.
    /// Returns `extended` unchanged when the full request already fits; otherwise the
    /// largest char-boundary prefix that fits, with [`TRUNCATION_MARKER`] appended.
    /// `fixed` is every other param (measured alongside, but never trimmed). When `fixed`
    /// alone is over `budget`, the note is fitted to [`PINBOARD_URL_LIMIT`] instead, and
    /// dropped when not even the marker fits under it.
    fn fit_extended(&self, fixed: &[(&str, &str)], extended: &str, budget: usize) -> String {
        let url_len = |ext: &str| self.add_url_len(fixed, ext);
        let room = if url_len("") > budget {
            PINBOARD_URL_LIMIT
        } else {
            budget
        };

        if url_len(extended) <= room {
            return extended.to_string();
        }

        // Binary-search the char-boundary offsets for the longest prefix that, with
        // the marker appended, still fits. (URL-encoding expands bytes unevenly, so we
        // measure rather than compute a byte budget.)
        let boundaries: Vec<usize> = extended
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(extended.len()))
            .collect();
        let fitting = boundaries.partition_point(|&cut| {
            url_len(&format!("{}{}", &extended[..cut], TRUNCATION_MARKER)) <= room
        });
        let best = fitting.checked_sub(1).map_or(0, |last| boundaries[last]);

        // Trim only when the marked result is actually shorter than the note, so a note
        // barely over budget isn't replaced by a longer, fabricated "[truncated]" note,
        // unless the note is over Pinboard's limit too.
        if best + TRUNCATION_MARKER.len() >= extended.len()
            && url_len(extended) <= PINBOARD_URL_LIMIT
        {
            return extended.to_string();
        }
        let marked = format!("{}{}", &extended[..best], TRUNCATION_MARKER);
        if url_len(&marked) <= PINBOARD_URL_LIMIT {
            marked
        } else {
            String::new()
        }
    }

    /// GET `posts/all` with the given `meta` flag (`"yes"` includes each entry's
    /// `meta`/`hash`), retrying transient failures. Shared by [`Self::all`], which parses
    /// the JSON, and [`Self::export_all`], which returns the body verbatim.
    async fn get_posts_all(&self, meta: &str) -> Result<reqwest::Response> {
        let params = [
            ("auth_token", self.auth_token.as_str()),
            ("format", "json"),
            ("meta", meta),
        ];
        self.get_with_backoff(&self.url("posts/all"), &params).await
    }

    /// GET a Pinboard endpoint, retrying transient failures (network errors,
    /// HTTP 429, 5xx) with backoff. Errors on a non-success final status.
    async fn get_with_backoff(
        &self,
        url: &str,
        params: &[(&str, &str)],
    ) -> Result<reqwest::Response> {
        let resp = self
            .http
            .send(url, || self.http.get(url).query(params))
            .await?;
        if !resp.status().is_success() {
            return Err(self.status_error(url, resp).await);
        }
        Ok(resp)
    }

    /// The error for a non-success response from `endpoint` (no query string), quoting
    /// the body only through [`redacted_error`].
    async fn status_error(&self, endpoint: &str, resp: reqwest::Response) -> anyhow::Error {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        redacted_error(
            &format!("Pinboard {endpoint} returned {status}"),
            &body,
            &self.auth_token,
        )
    }

    /// Fetch the raw `posts/all` JSON body verbatim (with `meta=yes`), for backups. Unlike
    /// [`Self::all`], this neither parses nor filters -- it returns exactly what Pinboard
    /// sends, preserving `meta`/`hash` and any entries `all` would skip.
    pub async fn export_all(&self) -> Result<String> {
        let resp = self.get_posts_all("yes").await?;
        resp.text().await.context("reading Pinboard posts/all body")
    }

    /// Construct a client pointed at an arbitrary API base, for tests. No inter-write
    /// pacing so `net_tests` don't sleep.
    #[cfg(test)]
    pub fn with_base_url(auth_token: String, base: String) -> Result<Self> {
        Self::build(auth_token, Duration::ZERO, base, client_builder())
    }

    #[cfg(test)]
    pub fn with_read_timeout(base: String, read_timeout: Duration) -> Result<Self> {
        Self::build(
            "user:tok".into(),
            Duration::ZERO,
            base,
            crate::http::client_builder_with_read_timeout(read_timeout),
        )
    }

    #[cfg(test)]
    pub fn rate_limit(&self) -> Duration {
        self.rate_limit
    }
}

impl BackupSource for PinboardClient {
    /// Pinboard is a backup target like any source, and like them it fetches **once**:
    /// `posts/all` is rate-limited to one call per five minutes, far longer than the
    /// retry budget, so a second call inside one run would 429 and fail the target. The
    /// `meta=yes` body is both the raw half and the input to the parsed half, which is
    /// also what makes the two describe the same instant.
    async fn dump(&self) -> Result<BackupDump, SourceError> {
        let body = self.export_all().await?;
        // Not `?`: a body that parses as JSON but no longer matches the wire shape (a
        // renamed `href`, say) is exactly when the raw half is worth keeping, so the
        // failure rides along in `records` and the raw file is still written.
        let records = serde_json::from_str::<Vec<PinboardBookmark>>(&body)
            .context("parsing Pinboard posts/all")
            .map(|wire| {
                wire_to_bookmarks(wire)
                    .iter()
                    .map(ExportBookmark::from)
                    .collect()
            });
        Ok(BackupDump {
            raw: vec![RawPage {
                stream: PRIMARY_STREAM,
                kind: RawKind::Json,
                body,
            }],
            records,
            // `posts/all` is unpaginated: one response is the whole account.
            truncated: None,
        })
    }
}

impl BookmarkStore for PinboardClient {
    /// An `item not found` answering a retry counts as done: the attempt before it may have
    /// deleted the bookmark and lost the response. On a first attempt it is an error.
    async fn delete(&self, url: &Url) -> Result<()> {
        let params = [
            ("url", url.as_str()),
            ("auth_token", self.auth_token.as_str()),
            ("format", "json"),
        ];
        let resp = self
            .get_with_backoff(&self.url("posts/delete"), &params)
            .await?;
        let retried = answered_a_retry(&resp);
        let parsed: AddResponse = resp
            .json()
            .await
            .context("parsing Pinboard posts/delete response")?;
        if retried && parsed.result_code == "item not found" {
            debug!("posts/delete retry found {url} already gone");
            return Ok(());
        }
        parsed.into_result("Pinboard posts/delete failed", &self.auth_token)
    }

    async fn add(&self, b: &Bookmark) -> Result<()> {
        self.post_add(b, OnTooLong::Halve).await.map(drop)
    }

    async fn update(&self, b: &Bookmark) -> Result<Update> {
        self.post_add(b, OnTooLong::Refuse).await
    }

    fn fit(&self, mut b: Bookmark) -> Bookmark {
        b.note = self.fitted_note(&b, MAX_URL_BYTES);
        b
    }

    fn kept_of_cut<'n>(&self, note: &'n str) -> Option<&'n str> {
        note.strip_suffix(TRUNCATION_MARKER)
    }
}

/// [`error_with_body`] for text from a Pinboard response, which could echo the request
/// line and the token in it. The token is redacted from the whole text first, so neither
/// the error nor the debug-logged body carries it, and the excerpt's cut can't leave half
/// a token behind.
fn redacted_error(what: &str, text: &str, auth_token: &str) -> anyhow::Error {
    error_with_body(what, &redact_token(text, auth_token), &[])
}

/// `text` with every occurrence of the `user:SECRET` token, and of its `SECRET` alone
/// (which also covers the token percent-encoded as `user%3ASECRET`), replaced, as is any
/// `auth_token=` value (a quoted request line cut short mid-token). Nothing else changes.
fn redact_token(text: &str, auth_token: &str) -> String {
    let secret = auth_token
        .split_once(':')
        .map_or(auth_token, |(_, secret)| secret);
    redact(&redact_auth_token_values(text), &[auth_token, secret])
}

/// `text` with the value of every `auth_token=` parameter replaced, up to the next `&`,
/// quote, `<` or whitespace.
fn redact_auth_token_values(text: &str) -> String {
    const KEY: &str = "auth_token=";
    let mut redacted = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(key_at) = rest.find(KEY) {
        let (before_value, value_onward) = rest.split_at(key_at + KEY.len());
        redacted.push_str(before_value);
        let value_len = value_onward
            .find(|c: char| matches!(c, '&' | '"' | '\'' | '<') || c.is_whitespace())
            .unwrap_or(value_onward.len());
        if value_len > 0 {
            redacted.push_str(REDACTED);
        }
        rest = &value_onward[value_len..];
    }
    redacted.push_str(rest);
    redacted
}

/// Convert wire bookmarks to the domain type, skipping (and warning on) any whose `href`
/// doesn't parse as a URL rather than aborting the whole run for one bad entry. An
/// unparseable `time` is not fatal -- the conversion keeps the bookmark with no timestamp
/// (see `Bookmark::try_from`) so it stays in the set sync dedups against.
fn wire_to_bookmarks(wire: Vec<PinboardBookmark>) -> Vec<Bookmark> {
    wire.into_iter()
        .filter_map(|b| {
            Bookmark::try_from(b)
                .map_err(|e| warn!("skipping bookmark with {e}"))
                .ok()
        })
        .collect()
}

/// Hermetic tests (no socket): `fit_extended` only builds URLs, never sends them.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{ELLIPSIS, ERROR_BODY_MAX_BYTES};

    fn test_client() -> PinboardClient {
        PinboardClient::with_base_url("user:tok".into(), PINBOARD_BASE.to_string()).unwrap()
    }

    /// A bookmark whose note is too long for a `posts/add` URL, dated `timestamp`.
    fn long_note_bookmark(timestamp: Option<time::OffsetDateTime>) -> Bookmark {
        Bookmark {
            note: "a long note ".repeat(800),
            timestamp,
            ..crate::test_support::stored_at("https://example.com/long")
        }
    }

    #[test]
    fn a_fitted_note_is_cut_once_and_then_left_alone() {
        let c = test_client();
        let once = c.fit(long_note_bookmark(None));
        assert!(once.note.ends_with(TRUNCATION_MARKER), "{}", once.note);
        assert_eq!(c.fit(once.clone()), once);
    }

    #[test]
    fn a_note_is_cut_the_same_whether_or_not_the_write_is_dated() {
        let c = test_client();
        let dated = long_note_bookmark(crate::timefmt::from_unix(1_700_000_000));
        assert_eq!(
            c.fit(long_note_bookmark(None)).note,
            c.fit(dated).note,
            "a dated cleanup write must not cut a note an undated sync wrote any further"
        );
    }

    #[test]
    fn a_note_is_cut_the_same_whatever_tags_repeat() {
        let c = test_client();
        let tagged = |tags: &[&str]| Bookmark {
            tags: crate::test_support::tag_list(tags),
            ..long_note_bookmark(None)
        };
        assert_eq!(
            c.fit(tagged(&["reddit", "rust", "reddit", "rust"])).note,
            c.fit(tagged(&["reddit", "rust"])).note,
            "the same tag set must cut the same, as `diff` compares tags as a set"
        );
    }

    #[test]
    fn a_note_past_half_the_request_line_is_kept_whole() {
        let c = test_client();
        let b = Bookmark {
            note: "a long note ".repeat(500),
            ..crate::test_support::stored_at("https://example.com/long")
        };
        assert_ne!(
            c.fitted_note(&b, MAX_URL_BYTES / 2),
            b.note,
            "the note must not fit half the budget"
        );
        assert_eq!(c.fit(b.clone()), b);
    }

    /// The length of the `posts/add` URL that sends `b`, measured with a `dt` as `fit` is.
    fn sent_url_len(c: &PinboardClient, b: &Bookmark) -> usize {
        let tags = b.tags.joined();
        let mut params = c.add_params(b, &tags, Some(DT_PLACEHOLDER));
        params.push(("extended", &b.note));
        c.http
            .get(c.url("posts/add"))
            .query(&params)
            .build()
            .unwrap()
            .url()
            .as_str()
            .len()
    }

    #[test]
    fn tags_are_kept_whole_and_the_note_cut_to_make_room() {
        let c = test_client();
        let few_tags = long_note_bookmark(None);
        let many_tags = Bookmark {
            tags: (0..100).map(|n| format!("tag-number-{n:03}")).collect(),
            ..few_tags.clone()
        };

        let fitted = c.fit(many_tags.clone());
        assert_eq!(
            Bookmark {
                note: many_tags.note.clone(),
                ..fitted.clone()
            },
            many_tags,
            "only the note is cut"
        );
        assert!(fitted.note.len() < c.fit(few_tags).note.len());
        assert!(sent_url_len(&c, &fitted) <= MAX_URL_BYTES);
    }

    /// A realistic set of fixed params (URL, title, tags, auth, ...).
    const FIXED: &[(&str, &str)] = &[
        (
            "url",
            "https://old.reddit.com/r/AutisticWithADHD/comments/1pmdzte/x/",
        ),
        ("description", "A representative post title"),
        ("tags", "reddit subreddit:autisticwithadhd"),
        ("replace", "yes"),
        ("shared", "no"),
        ("toread", "no"),
        ("auth_token", "user:tok"),
        ("format", "json"),
    ];

    /// Length of the URL reqwest builds for `extended` alongside `FIXED`.
    fn url_len(c: &PinboardClient, extended: &str) -> usize {
        let mut params = FIXED.to_vec();
        params.push(("extended", extended));
        c.http
            .get(c.url("posts/add"))
            .query(&params)
            .build()
            .unwrap()
            .url()
            .as_str()
            .len()
    }

    #[test]
    fn redacts_the_token_plain_percent_encoded_and_bare() {
        let body = "encoded user%3ASECRET123\ntoken user:SECRET123, secret SECRET123";
        assert_eq!(
            redact_token(body, "user:SECRET123"),
            "encoded user%3A[redacted]\ntoken [redacted], secret [redacted]"
        );
    }

    #[test]
    fn redacts_an_auth_token_value_cut_short() {
        assert_eq!(
            redact_token(
                "<p>GET /v1/posts/add?auth_token=user%3ASECR</p> auth_token=&x",
                "user:SECRET123"
            ),
            "<p>GET /v1/posts/add?auth_token=[redacted]</p> auth_token=&x"
        );
    }

    #[test]
    fn without_a_secret_part_redacts_nothing_extra() {
        assert_eq!(redact_token("bad  request", "user:"), "bad  request");
        assert_eq!(redact_token("bad request", ""), "bad request");
    }

    /// `redact_token`'s output is the body `error_with_body` logs at debug level, so it
    /// must be the whole text, uncut, less only the token.
    #[test]
    fn debug_logged_body_is_whole_but_has_no_token() {
        let body = format!(
            "<html>\n<p>GET /posts/all?auth_token=user%3ASECRET123&format=json</p>\n\
             <pre>{}</pre>\n<p>token: user:SECRET123</p>\n</html>",
            "x".repeat(2 * ERROR_BODY_MAX_BYTES)
        );
        let logged = redact_token(&body, "user:SECRET123");
        assert!(!logged.contains("SECRET123"), "token logged: {logged}");
        assert_eq!(
            logged,
            body.replace("user%3ASECRET123", "[redacted]")
                .replace("user:SECRET123", "[redacted]")
        );
    }

    #[test]
    fn redacted_error_redacts_before_cutting() {
        // Cutting the raw body first would keep `user:SECRET` and drop the `123`, so
        // neither the token nor its secret would match any more.
        let padding = "x".repeat(ERROR_BODY_MAX_BYTES - ELLIPSIS.len() - "user:SECRET".len());
        let body = format!("{padding}user:SECRET123{}", "y".repeat(20));
        let message = redacted_error("Pinboard posts/add failed", &body, "user:SECRET123");
        assert!(
            !message.to_string().contains("SEC"),
            "token prefix kept: {message}"
        );
    }

    #[test]
    fn short_notes_pass_through_unchanged() {
        let c = test_client();
        let notes = "Thread: https://old.reddit.com/r/x/\n\nA few sentences of body text.";
        assert_eq!(c.fit_extended(FIXED, notes, MAX_URL_BYTES), notes);
    }

    #[test]
    fn oversized_notes_are_trimmed_within_budget() {
        let c = test_client();
        let huge = "word ".repeat(20_000); // ~100 KB of notes
        let fitted = c.fit_extended(FIXED, &huge, MAX_URL_BYTES);
        assert!(fitted.len() < huge.len());
        assert!(fitted.ends_with(TRUNCATION_MARKER), "{fitted:?}");
        assert!(url_len(&c, &fitted) <= MAX_URL_BYTES);
    }

    #[test]
    fn trims_on_a_char_boundary_for_multibyte_notes() {
        let c = test_client();
        let huge = "é".repeat(20_000); // 2 bytes each -- a naive byte cut would panic
        let fitted = c.fit_extended(FIXED, &huge, MAX_URL_BYTES);
        assert!(fitted.ends_with(TRUNCATION_MARKER), "{fitted:?}");
        assert!(url_len(&c, &fitted) <= MAX_URL_BYTES);
    }

    #[test]
    fn a_smaller_budget_trims_more() {
        let c = test_client();
        let huge = "word ".repeat(20_000);
        let big = c.fit_extended(FIXED, &huge, MAX_URL_BYTES);
        let small = c.fit_extended(FIXED, &huge, MIN_URL_BYTES);
        assert!(small.len() < big.len());
        assert!(url_len(&c, &small) <= MIN_URL_BYTES);
    }

    #[test]
    fn the_414_retry_halves_down_to_its_floor_and_no_further() {
        let budgets: Vec<usize> =
            std::iter::successors(Some(MAX_URL_BYTES), |&budget| halved(budget)).collect();
        assert_eq!(budgets, [8100, 4050, 2025, 1012, 506]);
        assert_eq!(halved(2 * MIN_URL_BYTES), Some(MIN_URL_BYTES));
        assert_eq!(halved(2 * MIN_URL_BYTES - 1), None);
    }

    #[test]
    fn kept_of_cut_is_the_note_before_the_marker() {
        let c = test_client();
        let cut = format!("the kept part{TRUNCATION_MARKER}");
        assert_eq!(c.kept_of_cut(&cut), Some("the kept part"));
        assert_eq!(c.kept_of_cut(TRUNCATION_MARKER), Some(""));
        assert_eq!(c.kept_of_cut("a whole note"), None);
    }

    /// A note whose cut, marker included, would be exactly as long as the note is kept
    /// whole; one byte less budget and it is cut.
    #[test]
    fn a_cut_no_shorter_than_the_note_keeps_it_whole() {
        let c = test_client();
        // A euro sign is 3 bytes, 9 encoded: the 15-byte marker costs as much as 5.
        let note = "€".repeat(40);
        let marker_cost = url_len(&c, TRUNCATION_MARKER) - url_len(&c, "");
        let kept_chars = 35;
        let budget = url_len(&c, "") + 9 * kept_chars + marker_cost;
        assert_eq!(
            3 * kept_chars + TRUNCATION_MARKER.len(),
            note.len(),
            "the 35-char cut plus the marker is exactly the note's length"
        );

        assert_eq!(c.fit_extended(FIXED, &note, budget), note);
        let one_char_less = c.fit_extended(FIXED, &note, budget - 1);
        assert_eq!(
            one_char_less,
            format!("{}{TRUNCATION_MARKER}", "€".repeat(kept_chars - 1))
        );
    }

    #[test]
    fn a_budget_the_fixed_params_fill_exactly_cuts_the_whole_note() {
        let c = test_client();
        let budget = url_len(&c, "");
        let long = "A full paragraph of notes that is well past the marker length.";

        assert_eq!(c.fit_extended(FIXED, long, budget), TRUNCATION_MARKER);
    }

    /// `FIXED` with a title of `t`s long enough that its URL without a note is `len` bytes.
    fn fixed_of(c: &PinboardClient, len: usize) -> Vec<(&'static str, String)> {
        let title = "t".repeat(len - url_len(c, "") + FIXED[1].1.len());
        FIXED
            .iter()
            .map(|&(key, value)| match key {
                "description" => (key, title.clone()),
                _ => (key, value.to_string()),
            })
            .collect()
    }

    /// `fit_extended` over [`fixed_of`]'s params, with the URL it would send.
    fn fit_with_fields_of(c: &PinboardClient, len: usize, note: &str) -> (String, usize) {
        let owned = fixed_of(c, len);
        let fixed: Vec<(&str, &str)> = owned.iter().map(|(k, v)| (*k, v.as_str())).collect();
        assert_eq!(c.add_url_len(&fixed, ""), len);
        let fitted = c.fit_extended(&fixed, note, MAX_URL_BYTES);
        let sent = c.add_url_len(&fixed, &fitted);
        (fitted, sent)
    }

    #[test]
    fn fields_over_the_budget_fit_the_note_to_pinboards_limit() {
        let c = test_client();
        let long = "word ".repeat(2000);

        let (fitted, sent) = fit_with_fields_of(&c, MAX_URL_BYTES + 1, &long);
        assert!(fitted.ends_with(TRUNCATION_MARKER), "{fitted:?}");
        assert!(fitted.len() > TRUNCATION_MARKER.len(), "{fitted:?}");
        assert!(sent <= PINBOARD_URL_LIMIT && sent > MAX_URL_BYTES, "{sent}");

        assert_eq!(fit_with_fields_of(&c, MAX_URL_BYTES + 1, "hi").0, "hi");
    }

    #[test]
    fn a_note_not_even_the_marker_fits_under_the_limit_is_dropped() {
        let c = test_client();
        let long = "word ".repeat(2000);
        let marker_cost = url_len(&c, TRUNCATION_MARKER) - url_len(&c, "");

        assert_eq!(
            fit_with_fields_of(&c, PINBOARD_URL_LIMIT - marker_cost, &long),
            (TRUNCATION_MARKER.to_string(), PINBOARD_URL_LIMIT)
        );
        let room_for_less = PINBOARD_URL_LIMIT - marker_cost + 1;
        assert_eq!(fit_with_fields_of(&c, room_for_less, &long).0, "");
        assert_eq!(
            fit_with_fields_of(&c, room_for_less, "€€€€").0,
            "",
            "a note shorter than the marker but over the limit is dropped too"
        );
        assert_eq!(fit_with_fields_of(&c, room_for_less, "hi").0, "hi");
        assert_eq!(
            fit_with_fields_of(&c, PINBOARD_URL_LIMIT, "").1,
            PINBOARD_URL_LIMIT
        );
        assert_eq!(fit_with_fields_of(&c, PINBOARD_URL_LIMIT + 1, &long).0, "");
    }

    fn wire(time: &str) -> PinboardBookmark {
        PinboardBookmark {
            url: "https://example.com/".into(),
            description: "T".into(),
            extended: String::new(),
            tags: String::new(),
            time: time.into(),
            shared: "no".into(),
            toread: "no".into(),
        }
    }

    #[test]
    fn unparseable_time_keeps_the_bookmark_with_no_timestamp() {
        // A non-empty `time` that won't parse must NOT drop the record: it stays visible
        // (as `timestamp: None`) so sync's dedup set still contains its URL.
        let bookmark = Bookmark::try_from(wire("not a date")).unwrap();
        assert_eq!(bookmark.url.as_str(), "https://example.com/");
        assert_eq!(bookmark.timestamp, None);
    }

    #[test]
    fn parseable_time_becomes_a_timestamp() {
        let bookmark = Bookmark::try_from(wire("2020-01-01T00:00:00Z")).unwrap();
        assert_eq!(bookmark.timestamp, crate::timefmt::from_unix(1_577_836_800));
    }

    #[test]
    fn unparseable_url_is_the_only_fatal_conversion() {
        let mut bad = wire("");
        bad.url = "not a url".into();
        Bookmark::try_from(bad).unwrap_err();
    }

    #[tokio::test]
    async fn a_bookmark_with_an_unparseable_url_is_skipped_with_a_warning() {
        let mut bad = wire("");
        bad.url = "not a url".into();
        let (kept, logs) =
            crate::test_support::capturing_logs(async { wire_to_bookmarks(vec![bad, wire("")]) })
                .await;

        assert_eq!(kept.len(), 1);
        assert_eq!(
            crate::test_support::warnings(&logs),
            ["WARN skipping bookmark with unparseable URL not a url: relative URL without a base"]
        );
    }

    #[test]
    fn only_a_yes_flag_reads_as_true() {
        // A `shared=yes` read as private would make cleanup write every shared bookmark
        // back as private.
        let flagged = Bookmark::try_from(PinboardBookmark {
            shared: "yes".into(),
            toread: "yes".into(),
            ..wire("")
        })
        .unwrap();
        assert!(flagged.public && flagged.read_later);
        for other in ["no", ""] {
            let unflagged = Bookmark::try_from(PinboardBookmark {
                shared: other.into(),
                toread: other.into(),
                ..wire("")
            })
            .unwrap();
            assert!(!unflagged.public && !unflagged.read_later, "{other:?}");
        }
    }
}

/// Integration tests against a `wiremock` server. These bind a TCP socket, so
/// they can't run in the Nix build sandbox -- the flake skips `net_tests` there.
#[cfg(test)]
mod net_tests;
