//! Shared HTTP retry helper used by every client.

use std::cell::Cell;
use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result};
use time::format_description::well_known::Rfc2822;
use time::OffsetDateTime;

/// The waits before each retry of a request that could not establish a connection (a
/// DNS, TCP or TLS failure). Exponential and long, about 2.5 minutes of waiting in all,
/// so a resolver or network that is down for a minute or two is ridden out. The attempts
/// themselves add to that: no client sets a `connect_timeout`, so against a host that
/// drops SYNs each one blocks for the OS connect timeout. Not every connect failure
/// clears (a bad certificate, a wrong base URL); those also wait the schedule out before
/// failing.
const CONNECT_BACKOFF: [Duration; 5] = [
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(20),
    Duration::from_secs(40),
    Duration::from_secs(80),
];

/// One client's allowance for riding out connect failures on [`CONNECT_BACKOFF`].
///
/// It is spent the first time a request waits out the whole schedule and still cannot
/// connect: the outage has then outlasted what the schedule covers, and paying ~2.5
/// minutes again for every later item would stretch a `cleanup` over hundreds of
/// bookmarks into hours. From then on that client's connect errors take the linear
/// policy like any other transport error. It is never refilled, so a client gets one
/// long wait per run.
#[derive(Default)]
pub struct ConnectBudget {
    spent: Cell<bool>,
}

/// What a request's retry loop does with a response, as a classifier passed to
/// [`send_retrying_with`] decides it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Return it to the caller.
    Done,
    /// Transient: retry on the linear backoff.
    Backoff,
    /// Retry once after this wait; return it if it comes back again.
    RetryOnceAfter(Duration),
}

/// Retry an async operation. While `budget` is unspent, an `Err` for which
/// `is_connect_error` is true is retried after each delay in `connect_backoff` in turn;
/// once they run out the error is returned and `budget` is spent. Any other `Err` (a
/// transport failure, or a connect failure once `budget` is spent), or an `Ok(value)`
/// that `classify` calls [`Verdict::Backoff`] (a transient response, e.g. HTTP 5xx), is
/// retried with linear backoff (`base_delay * n`) for up to `max_attempts` such
/// attempts. An `Ok(value)` it calls [`Verdict::RetryOnceAfter`] is retried after that
/// wait the first time and returned the second. Otherwise returns the value as-is. The
/// budgets are separate, so one request can make up to
/// `connect_backoff.len() + max_attempts + 1` attempts.
///
/// Transport-agnostic so it can be unit-tested without any network — see tests.
async fn retry<T, E, F, Fut>(
    budget: &ConnectBudget,
    max_attempts: u32,
    base_delay: Duration,
    connect_backoff: &[Duration],
    classify: impl Fn(&T) -> Verdict,
    is_connect_error: impl Fn(&E) -> bool,
    mut op: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut connect_delays = connect_backoff.iter().copied();
    let mut other_attempts: u32 = 0;
    let mut waited_once = false;
    loop {
        let delay = match op().await {
            Err(err) if !budget.spent.get() && is_connect_error(&err) => {
                match connect_delays.next() {
                    Some(delay) => delay,
                    None => {
                        budget.spent.set(true);
                        return Err(err);
                    }
                }
            }
            outcome => {
                let verdict = match &outcome {
                    Ok(value) => classify(value),
                    Err(_) => Verdict::Backoff,
                };
                match verdict {
                    Verdict::Done => return outcome,
                    Verdict::RetryOnceAfter(_) if waited_once => return outcome,
                    Verdict::RetryOnceAfter(wait) => {
                        waited_once = true;
                        wait
                    }
                    Verdict::Backoff => {
                        other_attempts += 1;
                        if other_attempts >= max_attempts {
                            return outcome;
                        }
                        base_delay * other_attempts
                    }
                }
            }
        };
        tokio::time::sleep(delay).await;
    }
}

/// The wait a `Retry-After` header asks for, as of `now`: delta-seconds, or an HTTP-date
/// (one already past is no wait). `None` when the header is absent or unparseable.
fn retry_after(headers: &reqwest::header::HeaderMap, now: OffsetDateTime) -> Option<Duration> {
    let value = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let at = OffsetDateTime::parse(value, &Rfc2822).ok()?;
    Some((at - now).try_into().unwrap_or(Duration::ZERO))
}

/// The default classifier, which [`send_retrying`] uses: a 429 or 5xx is transient and
/// backs off; anything else goes back to the caller.
pub fn backoff_if_transient(resp: &reqwest::Response) -> Verdict {
    status_verdict(resp.status())
}

fn status_verdict(status: reqwest::StatusCode) -> Verdict {
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS || status.is_server_error() {
        Verdict::Backoff
    } else {
        Verdict::Done
    }
}

/// A classifier that retries a 429 at most once, after the wait its `Retry-After` asks
/// for, and only when that is at most `max_wait`; any other 429 goes back to the caller.
/// Every other status is classified as by [`backoff_if_transient`].
pub fn retry_after_up_to(max_wait: Duration) -> impl Fn(&reqwest::Response) -> Verdict {
    move |resp| {
        retry_after_verdict(
            resp.status(),
            resp.headers(),
            max_wait,
            OffsetDateTime::now_utc(),
        )
    }
}

fn retry_after_verdict(
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    max_wait: Duration,
    now: OffsetDateTime,
) -> Verdict {
    if status != reqwest::StatusCode::TOO_MANY_REQUESTS {
        return status_verdict(status);
    }
    match retry_after(headers, now) {
        Some(wait) if wait <= max_wait => Verdict::RetryOnceAfter(wait),
        _ => Verdict::Done,
    }
}

/// The starting point for every client's `reqwest::Client`: HTTP/1.1 only, so the TLS
/// ClientHello offers ALPN `http/1.1` alone. Reddit's edge 403s a ClientHello with no ALPN
/// extension (#78), and `native-tls-alpn` otherwise offers `h2` too, which this build
/// (no `http2` feature) cannot speak if a server picks it.
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder().http1_only()
}

/// Send a request, retrying *transient* failures. A request that could not establish a
/// connection (per `reqwest::Error::is_connect`: any connector failure, so DNS, TCP
/// connect, or the TLS handshake) is retried on the long exponential
/// [`CONNECT_BACKOFF`] (5s, 10s, 20s, 40s, 80s), so a short resolver or network outage
/// doesn't fail the run — once per client, while its `budget` is unspent (see
/// [`ConnectBudget`]). Other transport errors (timeouts, dropped connections), HTTP
/// 429, and 5xx are retried with linear backoff, `base_delay * n` for up to
/// `max_attempts` attempts. Responses that are not transient — 2xx and other 4xx (e.g.
/// 401/403/400) — are returned as-is for the caller to interpret, since retrying them
/// would not help.
///
/// `build` is called once per attempt, so it must reconstruct the request (the
/// request is consumed by `send`).
///
/// A transport error is returned without its request URL, for every client: a URL can
/// carry a credential in its query (Pinboard's `auth_token`), and `reqwest::Error` would
/// otherwise print it in both `Display` and `Debug`. `label` names the endpoint instead.
pub async fn send_retrying<F>(
    label: &str,
    budget: &ConnectBudget,
    max_attempts: u32,
    base_delay: Duration,
    build: F,
) -> Result<reqwest::Response>
where
    F: FnMut() -> reqwest::RequestBuilder,
{
    send_retrying_with(
        label,
        budget,
        max_attempts,
        base_delay,
        backoff_if_transient,
        build,
    )
    .await
}

/// [`send_retrying`], with `classify` deciding how each response is retried instead of
/// [`backoff_if_transient`]. A response it calls [`Verdict::Done`] comes back as-is, for
/// the caller to report.
pub async fn send_retrying_with<F>(
    label: &str,
    budget: &ConnectBudget,
    max_attempts: u32,
    base_delay: Duration,
    classify: impl Fn(&reqwest::Response) -> Verdict,
    build: F,
) -> Result<reqwest::Response>
where
    F: FnMut() -> reqwest::RequestBuilder,
{
    send_with_connect_backoff(
        label,
        budget,
        max_attempts,
        base_delay,
        &CONNECT_BACKOFF,
        classify,
        build,
    )
    .await
}

async fn send_with_connect_backoff<F>(
    label: &str,
    budget: &ConnectBudget,
    max_attempts: u32,
    base_delay: Duration,
    connect_backoff: &[Duration],
    classify: impl Fn(&reqwest::Response) -> Verdict,
    mut build: F,
) -> Result<reqwest::Response>
where
    F: FnMut() -> reqwest::RequestBuilder,
{
    let attempts = Cell::new(0u32);
    retry(
        budget,
        max_attempts,
        base_delay,
        connect_backoff,
        classify,
        reqwest::Error::is_connect,
        || {
            attempts.set(attempts.get() + 1);
            build().send()
        },
    )
    .await
    .map_err(reqwest::Error::without_url)
    .with_context(|| format!("{label} failed after {} attempts", attempts.get()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const NO_DELAY: Duration = Duration::from_millis(0);
    const NO_CONNECT_BACKOFF: [Duration; 3] = [NO_DELAY; 3];

    /// A failed attempt: `Connect` could not establish a connection, `Transport` failed
    /// after one was made (e.g. a dropped connection).
    #[derive(Debug, PartialEq)]
    enum Failure {
        Connect,
        Transport,
    }

    fn backoff_on_429_and_5xx(status: &u16) -> Verdict {
        if *status == 429 || (500..600).contains(status) {
            Verdict::Backoff
        } else {
            Verdict::Done
        }
    }

    /// The [`retry_after_up_to`] shape, with every 429 carrying a short wait.
    fn retry_429_once(status: &u16) -> Verdict {
        match *status {
            429 => Verdict::RetryOnceAfter(NO_DELAY),
            500..600 => Verdict::Backoff,
            _ => Verdict::Done,
        }
    }

    /// Retry over a canned sequence of outcomes (status code, or `Err`), counting
    /// how many attempts were made. 429/5xx are transient, and `Failure::Connect` gets
    /// the three-step `NO_CONNECT_BACKOFF` on a fresh budget.
    async fn run(outcomes: Vec<Result<u16, Failure>>, max: u32) -> (Result<u16, Failure>, usize) {
        run_on(&ConnectBudget::default(), outcomes, max).await
    }

    /// [`run`], drawing on a `budget` shared with other calls, as one client's requests do.
    async fn run_on(
        budget: &ConnectBudget,
        outcomes: Vec<Result<u16, Failure>>,
        max: u32,
    ) -> (Result<u16, Failure>, usize) {
        run_classified(budget, outcomes, max, backoff_on_429_and_5xx).await
    }

    /// [`run_on`], with `classify` deciding how each status is retried.
    async fn run_classified(
        budget: &ConnectBudget,
        outcomes: Vec<Result<u16, Failure>>,
        max: u32,
        classify: fn(&u16) -> Verdict,
    ) -> (Result<u16, Failure>, usize) {
        let calls = RefCell::new(0usize);
        let seq = RefCell::new(outcomes.into_iter());
        let result = retry(
            budget,
            max,
            NO_DELAY,
            &NO_CONNECT_BACKOFF,
            classify,
            |failure: &Failure| *failure == Failure::Connect,
            || {
                *calls.borrow_mut() += 1;
                let next = seq.borrow_mut().next().expect("ran out of canned outcomes");
                async move { next }
            },
        )
        .await;
        (result, calls.into_inner())
    }

    #[tokio::test]
    async fn a_retry_after_wait_is_taken_once() {
        let budget = ConnectBudget::default();
        let once = |outcomes| run_classified(&budget, outcomes, 5, retry_429_once);

        let (result, calls) = once(vec![Ok(429), Ok(200)]).await;
        assert_eq!(result, Ok(200));
        assert_eq!(calls, 2);

        let (result, calls) = once(vec![Ok(429), Ok(429)]).await;
        assert_eq!(result, Ok(429), "a second 429 goes back to the caller");
        assert_eq!(calls, 2);

        let (result, calls) = once(vec![Ok(503), Ok(429), Ok(503), Ok(429)]).await;
        assert_eq!(result, Ok(429), "5xx retries don't reset the one wait");
        assert_eq!(calls, 4);
    }

    #[tokio::test]
    async fn a_retry_after_wait_keeps_the_linear_budget() {
        let (result, calls) = run_classified(
            &ConnectBudget::default(),
            vec![Ok(503), Ok(429), Ok(503), Ok(200)],
            3,
            retry_429_once,
        )
        .await;
        assert_eq!(result, Ok(200));
        assert_eq!(calls, 4);
    }

    #[tokio::test(start_paused = true)]
    async fn a_retry_after_wait_sleeps_that_long() {
        let start = tokio::time::Instant::now();
        let attempt_offsets = RefCell::new(Vec::new());
        let statuses = RefCell::new(vec![429u16, 200].into_iter());
        let result: Result<u16, Failure> = retry(
            &ConnectBudget::default(),
            5,
            Duration::from_secs(2),
            &NO_CONNECT_BACKOFF,
            |status: &u16| match *status {
                429 => Verdict::RetryOnceAfter(Duration::from_secs(90)),
                _ => Verdict::Done,
            },
            |failure: &Failure| *failure == Failure::Connect,
            || {
                attempt_offsets.borrow_mut().push(start.elapsed().as_secs());
                let status = statuses.borrow_mut().next().unwrap();
                async move { Ok(status) }
            },
        )
        .await;

        assert_eq!(result, Ok(200));
        assert_eq!(attempt_offsets.into_inner(), [0, 90]);
    }

    fn headers(pairs: &[(&str, &str)]) -> reqwest::header::HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| {
                (
                    reqwest::header::HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    reqwest::header::HeaderValue::from_str(value).unwrap(),
                )
            })
            .collect()
    }

    fn at(rfc3339: &str) -> OffsetDateTime {
        crate::timefmt::parse_rfc3339(rfc3339).unwrap()
    }

    #[test]
    fn retry_after_reads_seconds_and_http_dates() {
        let now = at("2026-10-25T05:30:00Z");
        assert_eq!(
            retry_after(&headers(&[("retry-after", "120")]), now),
            Some(Duration::from_secs(120))
        );
        assert_eq!(
            retry_after(
                &headers(&[("retry-after", "Sun, 25 Oct 2026 05:31:30 GMT")]),
                now
            ),
            Some(Duration::from_secs(90))
        );
        assert_eq!(
            retry_after(
                &headers(&[("retry-after", "Sun, 25 Oct 2026 05:29:00 GMT")]),
                now
            ),
            Some(Duration::ZERO),
            "a date already past asks for no wait"
        );
        assert_eq!(retry_after(&headers(&[]), now), None);
        assert_eq!(retry_after(&headers(&[("retry-after", "soon")]), now), None);
        assert_eq!(retry_after(&headers(&[("retry-after", "-5")]), now), None);
    }

    #[test]
    fn a_429_is_retried_once_only_for_a_short_retry_after() {
        let now = at("2026-10-25T05:30:00Z");
        let max_wait = Duration::from_secs(120);
        let too_many = reqwest::StatusCode::TOO_MANY_REQUESTS;
        let verdict =
            |pairs: &[(&str, &str)]| retry_after_verdict(too_many, &headers(pairs), max_wait, now);

        assert_eq!(
            verdict(&[("retry-after", "120")]),
            Verdict::RetryOnceAfter(Duration::from_secs(120))
        );
        assert_eq!(
            verdict(&[("retry-after", "Sun, 25 Oct 2026 05:31:00 GMT")]),
            Verdict::RetryOnceAfter(Duration::from_secs(60))
        );
        assert_eq!(verdict(&[("retry-after", "121")]), Verdict::Done);
        assert_eq!(
            verdict(&[("retry-after", "Sun, 25 Oct 2026 06:30:00 GMT")]),
            Verdict::Done
        );
        assert_eq!(verdict(&[]), Verdict::Done);
    }

    fn response(status: u16, pairs: &[(&str, &str)]) -> reqwest::Response {
        let mut builder = http::Response::builder().status(status);
        for (name, value) in pairs {
            builder = builder.header(*name, *value);
        }
        reqwest::Response::from(builder.body("").unwrap())
    }

    #[test]
    fn the_classifiers_differ_only_on_a_429() {
        let honor = retry_after_up_to(Duration::from_secs(120));
        let short_429 = response(429, &[("retry-after", "30")]);

        assert_eq!(
            backoff_if_transient(&short_429),
            Verdict::Backoff,
            "every other client still backs off on a 429"
        );
        assert_eq!(
            honor(&short_429),
            Verdict::RetryOnceAfter(Duration::from_secs(30))
        );
        assert_eq!(honor(&response(429, &[])), Verdict::Done);
        for classify in [&backoff_if_transient as &dyn Fn(&_) -> _, &honor] {
            assert_eq!(classify(&response(502, &[])), Verdict::Backoff);
            assert_eq!(classify(&response(200, &[])), Verdict::Done);
            assert_eq!(classify(&response(403, &[])), Verdict::Done);
        }
    }

    #[tokio::test]
    async fn retries_transient_then_succeeds() {
        let (result, calls) = run(vec![Ok(503), Ok(429), Ok(200)], 5).await;
        assert_eq!(result, Ok(200));
        assert_eq!(calls, 3);
    }

    #[tokio::test]
    async fn retries_transport_error_then_succeeds() {
        let (result, calls) = run(vec![Err(Failure::Transport), Ok(200)], 5).await;
        assert_eq!(result, Ok(200));
        assert_eq!(calls, 2);
    }

    #[tokio::test]
    async fn does_not_retry_4xx_or_2xx() {
        let (result, calls) = run(vec![Ok(401)], 5).await;
        assert_eq!(result, Ok(401));
        assert_eq!(calls, 1);

        let (result, calls) = run(vec![Ok(200)], 5).await;
        assert_eq!(result, Ok(200));
        assert_eq!(calls, 1);
    }

    #[tokio::test]
    async fn gives_up_after_max_attempts() {
        // Persistent 5xx: returns the last response after exhausting attempts.
        let (result, calls) = run(vec![Ok(500), Ok(500), Ok(500)], 3).await;
        assert_eq!(result, Ok(500));
        assert_eq!(calls, 3);

        // Persistent transport error: returns the error after exhausting attempts.
        let (result, calls) = run(
            vec![
                Err(Failure::Transport),
                Err(Failure::Transport),
                Err(Failure::Transport),
            ],
            3,
        )
        .await;
        assert_eq!(result, Err(Failure::Transport));
        assert_eq!(calls, 3);
    }

    #[tokio::test]
    async fn connect_errors_are_retried_past_max_attempts() {
        let outcomes = vec![
            Err(Failure::Connect),
            Err(Failure::Connect),
            Err(Failure::Connect),
            Ok(200),
        ];
        let (result, calls) = run(outcomes, 1).await;
        assert_eq!(result, Ok(200));
        assert_eq!(calls, 4);
    }

    #[tokio::test]
    async fn gives_up_once_the_connect_backoff_runs_out() {
        let (result, calls) = run((0..4).map(|_| Err(Failure::Connect)).collect(), 5).await;
        assert_eq!(result, Err(Failure::Connect));
        assert_eq!(calls, 4);
    }

    #[tokio::test]
    async fn connect_retries_do_not_spend_the_linear_budget() {
        let outcomes = vec![
            Err(Failure::Connect),
            Err(Failure::Connect),
            Ok(503),
            Ok(200),
        ];
        let (result, calls) = run(outcomes, 2).await;
        assert_eq!(result, Ok(200));
        assert_eq!(calls, 4);

        let outcomes = vec![Err(Failure::Connect), Ok(503), Ok(503), Ok(200)];
        let (result, calls) = run(outcomes, 2).await;
        assert_eq!(result, Ok(503));
        assert_eq!(calls, 3);
    }

    #[tokio::test(start_paused = true)]
    async fn connect_backoff_outlasts_a_two_minute_outage() {
        let start = tokio::time::Instant::now();
        let attempt_offsets = RefCell::new(Vec::new());
        let result: Result<u16, Failure> = retry(
            &ConnectBudget::default(),
            5,
            Duration::from_secs(3),
            &CONNECT_BACKOFF,
            |_: &u16| Verdict::Done,
            |failure: &Failure| *failure == Failure::Connect,
            || {
                attempt_offsets.borrow_mut().push(start.elapsed().as_secs());
                async { Err(Failure::Connect) }
            },
        )
        .await;

        assert_eq!(result, Err(Failure::Connect));
        assert_eq!(attempt_offsets.into_inner(), [0, 5, 15, 35, 75, 155]);
    }

    #[tokio::test]
    async fn a_spent_connect_budget_falls_back_to_linear_retries() {
        let budget = ConnectBudget::default();
        let (result, calls) =
            run_on(&budget, (0..4).map(|_| Err(Failure::Connect)).collect(), 2).await;
        assert_eq!(result, Err(Failure::Connect));
        assert_eq!(
            calls, 4,
            "the first outage waits out the whole connect backoff"
        );

        let (result, calls) =
            run_on(&budget, (0..4).map(|_| Err(Failure::Connect)).collect(), 2).await;
        assert_eq!(result, Err(Failure::Connect));
        assert_eq!(
            calls, 2,
            "later requests on the client get only max_attempts"
        );

        let (result, calls) = run_on(&budget, vec![Err(Failure::Connect), Ok(200)], 2).await;
        assert_eq!(result, Ok(200));
        assert_eq!(
            calls, 2,
            "a spent budget still retries a connect error linearly"
        );
    }

    #[tokio::test]
    async fn a_connect_outage_that_clears_leaves_the_budget_unspent() {
        let budget = ConnectBudget::default();
        let (result, _) = run_on(&budget, vec![Err(Failure::Connect), Ok(200)], 2).await;
        assert_eq!(result, Ok(200));

        let (result, calls) =
            run_on(&budget, (0..4).map(|_| Err(Failure::Connect)).collect(), 2).await;
        assert_eq!(result, Err(Failure::Connect));
        assert_eq!(calls, 4);
    }

    /// Asserts that no rendering of a `send_retrying` error leaks `secret`, and that the
    /// error is a `reqwest::Error` matching `kind`, so the check can't pass on an
    /// unrelated failure that never carried the URL.
    pub(super) fn assert_error_hides(
        err: &anyhow::Error,
        kind: fn(&reqwest::Error) -> bool,
        secret: &str,
    ) {
        let renderings = [format!("{err}"), format!("{err:#}"), format!("{err:?}")]
            .into_iter()
            .chain(err.chain().map(|cause| format!("{cause:?}")));
        for rendering in renderings {
            assert!(!rendering.contains(secret), "token leaked: {rendering}");
        }
        assert!(
            err.downcast_ref::<reqwest::Error>().is_some_and(kind),
            "unexpected error: {err:#}"
        );
    }

    #[tokio::test]
    async fn request_error_does_not_leak_query_token() {
        let http = reqwest::Client::new();

        // reqwest rejects the scheme before opening a socket but still attaches the URL.
        let err = send_with_connect_backoff(
            "posts/all",
            &ConnectBudget::default(),
            2,
            NO_DELAY,
            &NO_CONNECT_BACKOFF,
            backoff_if_transient,
            || {
                http.get("ftp://api.pinboard.in/v1/posts/all")
                    .query(&[("auth_token", "user:SECRET123")])
            },
        )
        .await
        .unwrap_err();

        assert_error_hides(&err, reqwest::Error::is_builder, "SECRET123");
    }
}

/// Tests that need a socket; the flake skips `net_tests` in the Nix build sandbox.
#[cfg(test)]
mod net_tests {
    use super::tests::assert_error_hides;
    use super::*;

    /// The ALPN protocol names a TLS ClientHello offers, or `None` when it has no ALPN
    /// extension. `hello` starts at the TLS record header.
    fn offered_alpn(hello: &[u8]) -> Option<Vec<String>> {
        let u16_at = |at: usize| usize::from(u16::from_be_bytes([hello[at], hello[at + 1]]));
        assert_eq!(hello[0], 0x16, "not a TLS handshake record");
        assert_eq!(hello[5], 0x01, "not a ClientHello");
        let session_id = 5 + 4 + 2 + 32;
        let cipher_suites = session_id + 1 + usize::from(hello[session_id]);
        let compression = cipher_suites + 2 + u16_at(cipher_suites);
        let extensions = compression + 1 + usize::from(hello[compression]);
        let extensions_end = extensions + 2 + u16_at(extensions);
        assert!(
            hello.len() >= extensions_end,
            "ClientHello spans more than one record"
        );
        let mut at = extensions + 2;
        while at < extensions_end {
            let (kind, len) = (u16_at(at), u16_at(at + 2));
            let body = &hello[at + 4..at + 4 + len];
            if kind == 0x0010 {
                let mut names = Vec::new();
                let mut name_at = 2;
                while name_at < body.len() {
                    let name_len = usize::from(body[name_at]);
                    let name = &body[name_at + 1..name_at + 1 + name_len];
                    names.push(String::from_utf8_lossy(name).into_owned());
                    name_at += 1 + name_len;
                }
                return Some(names);
            }
            at += 4 + len;
        }
        None
    }

    #[tokio::test]
    async fn client_hello_offers_alpn_http1_only() {
        use std::io::Read;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        listener.set_nonblocking(true).unwrap();
        let read_hello = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            std::time::Instant::now() < deadline,
                            "the client never connected"
                        );
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(err) => panic!("accept: {err}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut hello = vec![0u8; 5];
            socket.read_exact(&mut hello).unwrap();
            let record_len = u16::from_be_bytes([hello[3], hello[4]]);
            hello.resize(5 + usize::from(record_len), 0);
            socket.read_exact(&mut hello[5..]).unwrap();
            hello
        });
        let http = client_builder().no_proxy().build().unwrap();

        let sent = http.get(format!("https://127.0.0.1:{port}/")).send().await;

        assert!(sent.is_err(), "the listener never completes the handshake");
        let hello = read_hello.join().unwrap();
        assert_eq!(offered_alpn(&hello), Some(vec!["http/1.1".to_string()]));
    }

    #[tokio::test]
    async fn connect_error_does_not_leak_query_token() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let http = reqwest::Client::new();
        let url = format!("http://127.0.0.1:{port}/v1/posts/all");

        let budget = ConnectBudget::default();
        let send = || {
            send_with_connect_backoff(
                "posts/all",
                &budget,
                2,
                Duration::ZERO,
                &[Duration::ZERO; 3],
                backoff_if_transient,
                || http.get(&url).query(&[("auth_token", "user:SECRET123")]),
            )
        };

        let err = send().await.unwrap_err();
        assert!(
            format!("{err:#}").contains("failed after 4 attempts"),
            "a refused connect must take the connect backoff: {err:#}"
        );
        assert_error_hides(&err, reqwest::Error::is_connect, "SECRET123");

        let err = send().await.unwrap_err();
        assert!(
            format!("{err:#}").contains("failed after 2 attempts"),
            "the spent budget must leave only the linear retries: {err:#}"
        );
    }
}
