//! Minimal Pinboard API client: `posts/add`, `posts/all`, and `posts/delete`,
//! with rate limiting and transient-failure retries (see [`crate::http`]).

use std::cell::Cell;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use log::warn;
use serde::Deserialize;
use url::Url;

use crate::bookmark::{Bookmark, BookmarkStore};
use crate::http::send_retrying;

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
    http: reqwest::Client,
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
        let http = reqwest::Client::builder()
            .user_agent(USER_AGENT)
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            http,
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
            tokio::time::sleep(Duration::from_secs(self.rate_limit_secs)).await;
        }
        self.wrote.set(true);
    }
}

impl BookmarkStore for PinboardClient {
    async fn all(&self) -> Result<Vec<Bookmark>> {
        let resp = self.get_posts_all("no").await?;
        let wire: Vec<PinboardBookmark> =
            resp.json().await.context("parsing Pinboard posts/all")?;
        // Skip (and warn on) any bookmark whose `href` doesn't parse as a URL rather than
        // aborting the whole run for one bad entry. An unparseable `time` is not fatal —
        // the conversion keeps the bookmark with no timestamp (see `Bookmark::try_from`)
        // so it stays in the set sync dedups against.
        Ok(wire
            .into_iter()
            .filter_map(|b| {
                let href = b.url.clone();
                Bookmark::try_from(b)
                    .map_err(|e| warn!("skipping bookmark with unparseable URL {href}: {e}"))
                    .ok()
            })
            .collect())
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
        if parsed.result_code != "done" {
            return Err(anyhow!(
                "Pinboard posts/delete failed: {}",
                parsed.result_code
            ));
        }
        Ok(())
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
        // the bookmark. Transient errors (network/429/5xx) are handled by send_retrying.
        let mut budget = MAX_URL_BYTES;
        loop {
            let extended = self.fit_extended(&endpoint, &fixed, &b.note, budget);
            let mut params = fixed.clone();
            params.push(("extended", extended.as_str()));

            let resp = send_retrying(
                &endpoint,
                MAX_RETRIES,
                Duration::from_secs(RATE_LIMIT_SECS),
                || self.http.get(&endpoint).query(&params),
            )
            .await?;

            if resp.status() == reqwest::StatusCode::URI_TOO_LONG && budget > MIN_URL_BYTES {
                budget /= 2;
                continue;
            }
            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                bail!("Pinboard {endpoint} returned {status}: {}", body.trim());
            }
            let parsed: AddResponse = resp
                .json()
                .await
                .context("parsing Pinboard posts/add response")?;
            if parsed.result_code != "done" {
                return Err(anyhow!("Pinboard posts/add failed: {}", parsed.result_code));
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

        // Binary-search the char-boundary offsets for the longest prefix that, with
        // the marker appended, still fits. (URL-encoding expands bytes unevenly, so we
        // measure rather than compute a byte budget.)
        let boundaries: Vec<usize> = extended
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(extended.len()))
            .collect();
        let fits = |cut: usize| url_len(&format!("{}{}", &extended[..cut], TRUNCATION_MARKER));
        let (mut lo, mut hi, mut best) = (0usize, boundaries.len() - 1, 0usize);
        while lo <= hi {
            let mid = (lo + hi) / 2;
            if fits(boundaries[mid]) <= budget {
                best = boundaries[mid];
                lo = mid + 1;
            } else if mid == 0 {
                break;
            } else {
                hi = mid - 1;
            }
        }

        // The fixed params alone can push the URL over budget; then no prefix of the note
        // is the cause of the overflow and the marker would only add bytes. Trim only when
        // the marked result is actually shorter than the note, so an empty or tiny note
        // isn't replaced by a fabricated "[truncated]" note.
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
        let resp = send_retrying(
            url,
            MAX_RETRIES,
            Duration::from_secs(RATE_LIMIT_SECS),
            || self.http.get(url).query(params),
        )
        .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            bail!("Pinboard {url} returned {status}: {}", body.trim());
        }
        Ok(resp)
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
    }
}

/// Integration tests against a `wiremock` server. These bind a TCP socket, so
/// they can't run in the Nix build sandbox — the flake skips `net_tests` there.
#[cfg(test)]
mod net_tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn client(server: &MockServer) -> PinboardClient {
        PinboardClient::with_base_url("user:tok".into(), server.uri()).unwrap()
    }

    #[tokio::test]
    async fn add_succeeds_on_done() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/posts/add"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"})))
            .expect(1)
            .mount(&server)
            .await;

        client(&server)
            .add(&Bookmark {
                url: Url::parse("https://old.reddit.com/r/x/").unwrap(),
                title: "Title".into(),
                note: String::new(),
                tags: vec!["reddit".into()],
                timestamp: None,
                public: false,
                read_later: false,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn add_retries_after_414_then_succeeds() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use wiremock::{Request, Respond};

        // 414 on the first hit (URL too long), then `done` — the client should shrink
        // the notes and retry rather than failing. A stateful responder makes the
        // sequence deterministic (two ambiguous mocks don't reliably order).
        struct FailFirst(AtomicUsize);
        impl Respond for FailFirst {
            fn respond(&self, _: &Request) -> ResponseTemplate {
                if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(414).set_body_string("Request URI is too long")
                } else {
                    ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"}))
                }
            }
        }

        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/posts/add"))
            .respond_with(FailFirst(AtomicUsize::new(0)))
            .expect(2) // exactly one retry after the 414
            .mount(&server)
            .await;

        client(&server)
            .add(&Bookmark {
                url: Url::parse("https://old.reddit.com/r/x/").unwrap(),
                title: "Title".into(),
                note: "long notes ".repeat(2000),
                tags: vec!["reddit".into()],
                timestamp: None,
                public: false,
                read_later: false,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn add_errors_on_non_done_result() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/posts/add"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"result_code": "missing url"})),
            )
            .mount(&server)
            .await;

        let err = client(&server)
            .add(&Bookmark {
                url: Url::parse("https://old.reddit.com/r/x/").unwrap(),
                title: "Title".into(),
                note: String::new(),
                tags: vec!["reddit".into()],
                timestamp: None,
                public: false,
                read_later: false,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing url"));
    }

    #[tokio::test]
    async fn all_parses_bookmarks() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/posts/all"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "href": "https://old.reddit.com/r/rust/comments/a/x/", "description": "A",
                  "extended": "", "tags": "reddit subreddit:rust", "time": "2020-01-01T00:00:00Z",
                  "shared": "no", "toread": "no" },
                { "href": "https://example.com/", "description": "E", "extended": "", "tags": "",
                  "time": "", "shared": "no", "toread": "no" }
            ])))
            .mount(&server)
            .await;

        let all = client(&server).all().await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(
            all[0].url.as_str(),
            "https://old.reddit.com/r/rust/comments/a/x/"
        );
        assert_eq!(all[0].tags, vec!["reddit", "subreddit:rust"]);
        // The ISO-8601 `time` is parsed into a timestamp on read.
        assert_eq!(all[0].timestamp, crate::timefmt::from_unix(1_577_836_800)); // 2020-01-01T00:00:00Z
        assert_eq!(all[1].timestamp, None); // empty time → no timestamp
    }

    #[tokio::test]
    async fn all_skips_bookmark_with_unparseable_href() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/posts/all"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "href": "not a url", "description": "bad", "extended": "", "tags": "",
                  "time": "", "shared": "no", "toread": "no" },
                { "href": "https://example.com/", "description": "E", "extended": "", "tags": "",
                  "time": "", "shared": "no", "toread": "no" }
            ])))
            .mount(&server)
            .await;

        // The unparseable href is skipped (logged); the valid bookmark still comes through.
        let all = client(&server).all().await.unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].url.as_str(), "https://example.com/");
    }

    #[tokio::test]
    async fn all_keeps_bookmark_with_unparseable_time() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/posts/all"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                { "href": "https://example.com/bad-time/", "description": "bad", "extended": "",
                  "tags": "", "time": "not a date", "shared": "no", "toread": "no" },
                { "href": "https://example.com/", "description": "E", "extended": "", "tags": "",
                  "time": "2020-01-01T00:00:00Z", "shared": "no", "toread": "no" }
            ])))
            .mount(&server)
            .await;

        // A bookmark whose (non-empty) time won't parse is kept with no timestamp, not
        // dropped: dropping it would evict its URL from sync's dedup set and make the next
        // `sync` re-add (and clobber) it. The valid bookmark still comes through too.
        let all = client(&server).all().await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].url.as_str(), "https://example.com/bad-time/");
        assert_eq!(all[0].timestamp, None);
        assert_eq!(all[1].url.as_str(), "https://example.com/");
        assert_eq!(all[1].timestamp, crate::timefmt::from_unix(1_577_836_800));
    }

    #[tokio::test]
    async fn export_all_returns_body_verbatim() {
        let server = MockServer::start().await;
        // Raw body carrying `meta`/`hash` and an unparseable `href` — both of which the
        // parsed `all()` path drops. `export_all` must return the bytes untouched, so
        // assert against a raw string (not re-serialized JSON).
        let body = r#"[{"href":"not a url","description":"bad","meta":"abc","hash":"def"},
{"href":"https://example.com/","description":"E","meta":"123","hash":"456"}]"#;
        Mock::given(method("GET"))
            .and(path("/posts/all"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;

        let dumped = client(&server).export_all().await.unwrap();
        assert_eq!(dumped, body);
    }

    #[tokio::test]
    async fn delete_succeeds_on_done() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/posts/delete"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"result_code": "done"})))
            .expect(1)
            .mount(&server)
            .await;

        client(&server)
            .delete(&Url::parse("https://old.reddit.com/r/x/").unwrap())
            .await
            .unwrap();
    }
}
