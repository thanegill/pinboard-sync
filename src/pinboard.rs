//! Minimal Pinboard API client: `posts/add`, `posts/all`, and `posts/delete`,
//! with rate limiting and transient-failure retries (see [`crate::http`]).

use std::cell::Cell;
use std::time::Duration;

use anyhow::{Context, Result};
use log::warn;
use serde::Deserialize;
use url::Url;

use crate::backup::{BackupDump, BackupSource, ExportBookmark, RawKind, RawPage};
use crate::bookmark::{Bookmark, BookmarkStore};
use crate::http::{client_builder, error_with_body, redact, RetryingClient, REDACTED};
use crate::source::SourceError;

const DEFAULT_BASE: &str = "https://api.pinboard.in/v1";
const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// Seconds to wait between successful `posts/add` calls (Pinboard asks for ~3s).
pub const RATE_LIMIT_SECS: u64 = 3;
const MAX_RETRIES: u32 = 5;

/// `posts/add` is a GET, so every field — notably `extended` (the notes) — rides in
/// the request URL. Servers cap the request line (Pinboard answers a long one with
/// `414 URI Too Long`), so we trim `extended` to keep the encoded URL within a byte
/// budget. This is the *starting* budget; Pinboard's exact limit is undocumented and
/// is lower than the common ~8 KB, so [`PinboardClient::post_add`] halves the budget
/// and retries if a 414 still comes back. Other fields are never trimmed.
const MAX_URL_BYTES: usize = 4000;
/// Floor for the adaptive 414 retry: below this we stop shrinking and surface the
/// error rather than writing a bookmark with essentially no notes in an endless loop.
const MIN_URL_BYTES: usize = 500;
/// Appended to a trimmed `extended` so the truncation is visible in the bookmark.
const TRUNCATION_MARKER: &str = "… [truncated]";

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

/// A bookmark exactly as `posts/all` returns it — the Pinboard wire shape, with its
/// space-joined tag string, ISO-8601 `time`, and `"yes"/"no"` flags. Converted to the
/// service-agnostic [`Bookmark`] on read (see the `From` impl below); nothing outside
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

// The service-agnostic domain form, `crate::bookmark::Bookmark`, is what the rest of the
// crate works with; `posts/all` parses into `PinboardBookmark` and converts via its
// `From` impl, and a write takes a whole `Bookmark` (mapped to the API params in
// `post_add`). The `BookmarkStore` port also lives in `bookmark`; this module is just the
// Pinboard client behind that port.

pub struct PinboardClient {
    http: RetryingClient,
    /// `username:TOKEN`.
    auth_token: String,
    /// Seconds to pause between successive `posts/add` writes.
    rate_limit_secs: u64,
    /// Set once the first write has happened, so [`Self::pace`] spaces only *successive*
    /// writes. Pacing is the client's own concern, not the generic `BookmarkStore` port's.
    /// (`Cell` is fine: the client is never used across threads — the futures aren't `Send`.)
    wrote: Cell<bool>,
    /// API base, e.g. `https://api.pinboard.in/v1`.
    base: String,
}

impl PinboardClient {
    pub fn new(auth_token: String, rate_limit_secs: u64) -> Result<Self> {
        Self::build(auth_token, rate_limit_secs, DEFAULT_BASE.to_string())
    }

    fn build(auth_token: String, rate_limit_secs: u64, base: String) -> Result<Self> {
        let http = client_builder()
            .user_agent(USER_AGENT)
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http: RetryingClient::new(http, MAX_RETRIES, Duration::from_secs(RATE_LIMIT_SECS)),
            auth_token,
            rate_limit_secs,
            wrote: Cell::new(false),
            base,
        })
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base)
    }

    /// Pinboard asks for ~3s between `posts/add` calls. Pause before each write *after*
    /// the first (a failed attempt still hit the API, so it counts).
    async fn pace(&self) {
        if self.wrote.get() {
            crate::http::sleep(Duration::from_secs(self.rate_limit_secs)).await;
        }
        self.wrote.set(true);
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
                stream: "",
                kind: RawKind::Json,
                body,
            }],
            records,
            // `posts/all` is unpaginated: one response is the whole account.
            truncated: None,
        })
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
/// unparseable `time` is not fatal — the conversion keeps the bookmark with no timestamp
/// (see `Bookmark::try_from`) so it stays in the set sync dedups against.
fn wire_to_bookmarks(wire: Vec<PinboardBookmark>) -> Vec<Bookmark> {
    wire.into_iter()
        .filter_map(|b| {
            let href = b.url.clone();
            Bookmark::try_from(b)
                .map_err(|e| warn!("skipping bookmark with unparseable URL {href}: {e}"))
                .ok()
        })
        .collect()
}

impl BookmarkStore for PinboardClient {
    async fn all(&self) -> Result<Vec<Bookmark>> {
        let resp = self.get_posts_all("no").await?;
        let body = resp.text().await.context("reading Pinboard posts/all")?;
        let wire: Vec<PinboardBookmark> =
            serde_json::from_str(&body).context("parsing Pinboard posts/all")?;
        Ok(wire_to_bookmarks(wire))
    }

    async fn delete(&self, url: &Url) -> Result<()> {
        let params = [
            ("url", url.as_str()),
            ("auth_token", self.auth_token.as_str()),
            ("format", "json"),
        ];
        let resp = self
            .get_with_backoff(&self.url("posts/delete"), &params)
            .await?;
        let parsed: AddResponse = resp
            .json()
            .await
            .context("parsing Pinboard posts/delete response")?;
        parsed.into_result("Pinboard posts/delete failed", &self.auth_token)
    }

    async fn add(&self, b: &Bookmark) -> Result<()> {
        self.post_add(b).await
    }

    async fn update(&self, b: &Bookmark) -> Result<()> {
        self.post_add(b).await
    }
}

impl PinboardClient {
    /// `posts/add` with `replace=yes`, mapping a domain [`Bookmark`] to the Pinboard
    /// parameters (`public`/`read_later` → `shared`/`toread`, `timestamp` → `dt`). A set
    /// `timestamp` sets the bookmark time. `extended` is trimmed if needed to keep the GET
    /// URL under [`MAX_URL_BYTES`].
    async fn post_add(&self, b: &Bookmark) -> Result<()> {
        // Space successive writes (Pinboard's rate limit) — internal to the client, so
        // the generic write loops don't manage pacing.
        self.pace().await;
        let tags = b.tags.join(" ");
        let dt = b
            .timestamp
            .and_then(crate::timefmt::to_rfc3339)
            .unwrap_or_default();
        let endpoint = self.url("posts/add");

        // Every param except `extended` — these are fixed (never trimmed) and set the
        // budget that `extended` has to fit within.
        let mut fixed = vec![
            ("url", b.url.as_str()),
            ("description", b.title.as_str()),
            ("tags", tags.as_str()),
            ("replace", "yes"),
            ("shared", if b.public { "yes" } else { "no" }),
            ("toread", if b.read_later { "yes" } else { "no" }),
            ("auth_token", self.auth_token.as_str()),
            ("format", "json"),
        ];
        if !dt.is_empty() {
            fixed.push(("dt", dt.as_str()));
        }

        // Trim the notes to the budget; if Pinboard still rejects the URL as too long
        // (its limit is undocumented), halve the budget and retry rather than dropping
        // the bookmark. Transient errors (network/429/5xx) are handled by
        // `RetryingClient::send`.
        let mut budget = MAX_URL_BYTES;
        loop {
            let extended = self.fit_extended(&endpoint, &fixed, &b.note, budget);
            let mut params = fixed.clone();
            params.push(("extended", extended.as_str()));

            let resp = self
                .http
                .send(&endpoint, || self.http.get(&endpoint).query(&params))
                .await?;

            if resp.status() == reqwest::StatusCode::URI_TOO_LONG && budget > MIN_URL_BYTES {
                budget /= 2;
                continue;
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
            // note is now shorter than the one `cleanup` planned, so a later merge sees a
            // block it doesn't recognise and can re-append it.
            if extended.len() < b.note.len() {
                warn!(
                    "note trimmed to fit the request URL for {}: {} bytes -> {}",
                    b.url,
                    b.note.len(),
                    extended.len()
                );
            }
            return Ok(());
        }
    }

    /// Trim `extended` so the encoded `posts/add` URL stays within `budget` bytes.
    /// Returns `extended` unchanged when the full request already fits; otherwise the
    /// largest char-boundary prefix that fits, with [`TRUNCATION_MARKER`] appended.
    /// `fixed` is every other param (measured alongside, but never trimmed).
    fn fit_extended(
        &self,
        endpoint: &str,
        fixed: &[(&str, &str)],
        extended: &str,
        budget: usize,
    ) -> String {
        // Byte length of the URL reqwest would build with this `extended` value.
        let url_len = |ext: &str| -> usize {
            let mut params = fixed.to_vec();
            params.push(("extended", ext));
            self.http
                .get(endpoint)
                .query(&params)
                .build()
                .map(|r| r.url().as_str().len())
                .unwrap_or(usize::MAX)
        };

        if url_len(extended) <= budget {
            return extended.to_string();
        }

        // Fixed params alone exceed budget: the note isn't the overflow cause and the
        // marker would only add bytes. Leave the note intact (the full request may still
        // fit Pinboard's real limit; if not, the normal retry path handles it).
        if url_len("") > budget {
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
            url_len(&format!("{}{}", &extended[..cut], TRUNCATION_MARKER)) <= budget
        });
        let best = fitting.checked_sub(1).map_or(0, |last| boundaries[last]);

        // Trim only when the marked result is actually shorter than the note, so a note
        // barely over budget isn't replaced by a longer, fabricated "[truncated]" note.
        if best + TRUNCATION_MARKER.len() >= extended.len() {
            return extended.to_string();
        }
        format!("{}{}", &extended[..best], TRUNCATION_MARKER)
    }

    /// GET `posts/all` with the given `meta` flag (`"yes"` includes each entry's
    /// `meta`/`hash`), retrying transient failures. Shared by [`BookmarkStore::all`], which
    /// parses the JSON, and [`Self::export_all`], which returns the body verbatim.
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

    /// Fetch the raw `posts/all` JSON body verbatim (with `meta=yes`), for backups.
    /// Unlike [`BookmarkStore::all`], this neither parses nor filters — it returns exactly
    /// what Pinboard sends, preserving `meta`/`hash` and any entries `all` would skip.
    pub async fn export_all(&self) -> Result<String> {
        let resp = self.get_posts_all("yes").await?;
        resp.text().await.context("reading Pinboard posts/all body")
    }

    /// Construct a client pointed at an arbitrary API base, for tests. No inter-write
    /// pacing so `net_tests` don't sleep.
    #[cfg(test)]
    pub fn with_base_url(auth_token: String, base: String) -> Result<Self> {
        Self::build(auth_token, 0, base)
    }
}

/// Hermetic tests (no socket): `fit_extended` only builds URLs, never sends them.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::{ELLIPSIS, ERROR_BODY_MAX_BYTES};

    fn test_client() -> PinboardClient {
        PinboardClient::with_base_url("user:tok".into(), DEFAULT_BASE.to_string()).unwrap()
    }

    /// A realistic set of fixed params (URL, title, tags, auth, …).
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
        assert_eq!(
            c.fit_extended(&c.url("posts/add"), FIXED, notes, MAX_URL_BYTES),
            notes
        );
    }

    #[test]
    fn oversized_notes_are_trimmed_within_budget() {
        let c = test_client();
        let huge = "word ".repeat(20_000); // ~100 KB of notes
        let fitted = c.fit_extended(&c.url("posts/add"), FIXED, &huge, MAX_URL_BYTES);
        assert!(fitted.len() < huge.len());
        assert!(fitted.ends_with(TRUNCATION_MARKER));
        assert!(url_len(&c, &fitted) <= MAX_URL_BYTES);
    }

    #[test]
    fn trims_on_a_char_boundary_for_multibyte_notes() {
        let c = test_client();
        let huge = "é".repeat(20_000); // 2 bytes each — a naive byte cut would panic
        let fitted = c.fit_extended(&c.url("posts/add"), FIXED, &huge, MAX_URL_BYTES);
        assert!(fitted.ends_with(TRUNCATION_MARKER));
        assert!(url_len(&c, &fitted) <= MAX_URL_BYTES);
    }

    #[test]
    fn a_smaller_budget_trims_more() {
        let c = test_client();
        let huge = "word ".repeat(20_000);
        let big = c.fit_extended(&c.url("posts/add"), FIXED, &huge, MAX_URL_BYTES);
        let small = c.fit_extended(&c.url("posts/add"), FIXED, &huge, MIN_URL_BYTES);
        assert!(small.len() < big.len());
        assert!(url_len(&c, &small) <= MIN_URL_BYTES);
    }

    #[test]
    fn note_untouched_when_fixed_params_alone_exceed_budget() {
        let c = test_client();
        // A budget the fixed params overrun on their own, so trimming the note can't help.
        let budget = url_len(&c, "").saturating_sub(1);

        assert_eq!(c.fit_extended(&c.url("posts/add"), FIXED, "", budget), "");

        let tiny = "hi";
        assert_eq!(
            c.fit_extended(&c.url("posts/add"), FIXED, tiny, budget),
            tiny
        );

        // A note longer than the 15-byte marker must survive intact too: since the fixed
        // params alone overrun the budget, trimming or marking it can't help, so replacing
        // it with the marker would be silent data loss.
        let long = "A full paragraph of notes that is well past the marker length.";
        assert!(long.len() > TRUNCATION_MARKER.len());
        assert_eq!(
            c.fit_extended(&c.url("posts/add"), FIXED, long, budget),
            long
        );
    }
}

/// Integration tests against a `wiremock` server. These bind a TCP socket, so
/// they can't run in the Nix build sandbox — the flake skips `net_tests` there.
#[cfg(test)]
mod net_tests;
