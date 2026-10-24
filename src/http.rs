//! Shared HTTP retry helper used by every client.

use std::cell::Cell;
use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result};

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

/// Retry an async operation. While `budget` is unspent, an `Err` for which
/// `is_connect_error` is true is retried after each delay in `connect_backoff` in turn;
/// once they run out the error is returned and `budget` is spent. Any other `Err` (a
/// transport failure, or a connect failure once `budget` is spent), or an `Ok(value)`
/// for which `retry_on_ok` is true (a transient response, e.g. HTTP 429/5xx), is
/// retried with linear backoff (`base_delay * n`) for up to `max_attempts` such
/// attempts. Otherwise returns the value as-is. The two budgets are separate, so one
/// request can make up to `connect_backoff.len() + max_attempts` attempts.
///
/// Transport-agnostic so it can be unit-tested without any network — see tests.
async fn retry<T, E, F, Fut>(
    budget: &ConnectBudget,
    max_attempts: u32,
    base_delay: Duration,
    connect_backoff: &[Duration],
    retry_on_ok: impl Fn(&T) -> bool,
    is_connect_error: impl Fn(&E) -> bool,
    mut op: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut connect_delays = connect_backoff.iter().copied();
    let mut other_attempts: u32 = 0;
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
                other_attempts += 1;
                let transient = match &outcome {
                    Ok(value) => retry_on_ok(value),
                    Err(_) => true,
                };
                if !transient || other_attempts >= max_attempts {
                    return outcome;
                }
                base_delay * other_attempts
            }
        };
        tokio::time::sleep(delay).await;
    }
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
    send_with_connect_backoff(
        label,
        budget,
        max_attempts,
        base_delay,
        &CONNECT_BACKOFF,
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
        |resp: &reqwest::Response| {
            resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                || resp.status().is_server_error()
        },
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

    /// Retry over a canned sequence of outcomes (status code, or `Err`), counting
    /// how many attempts were made. `retry_on_ok` treats 429/5xx as transient, and
    /// `Failure::Connect` gets the three-step `NO_CONNECT_BACKOFF` on a fresh budget.
    async fn run(outcomes: Vec<Result<u16, Failure>>, max: u32) -> (Result<u16, Failure>, usize) {
        run_on(&ConnectBudget::default(), outcomes, max).await
    }

    /// [`run`], drawing on a `budget` shared with other calls, as one client's requests do.
    async fn run_on(
        budget: &ConnectBudget,
        outcomes: Vec<Result<u16, Failure>>,
        max: u32,
    ) -> (Result<u16, Failure>, usize) {
        let calls = RefCell::new(0usize);
        let seq = RefCell::new(outcomes.into_iter());
        let result = retry(
            budget,
            max,
            NO_DELAY,
            &NO_CONNECT_BACKOFF,
            |s: &u16| *s == 429 || (500..600).contains(s),
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
            |_: &u16| false,
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
