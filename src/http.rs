//! Shared HTTP retry helper used by the Reddit and Pinboard clients.

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result};

/// Retry an async operation with linear backoff. Retries when `op` returns `Err`
/// (a transport failure) or returns `Ok(value)` for which `retry_on_ok` is true
/// (a transient response, e.g. HTTP 429/5xx). Otherwise returns the value as-is.
///
/// Transport-agnostic so it can be unit-tested without any network — see tests.
async fn retry<T, E, F, Fut>(
    max_attempts: u32,
    base_delay: Duration,
    retry_on_ok: impl Fn(&T) -> bool,
    mut op: F,
) -> Result<T, E>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, E>>,
{
    let mut attempt: u32 = 0;
    loop {
        attempt += 1;
        match op().await {
            Ok(value) if retry_on_ok(&value) && attempt < max_attempts => {}
            Ok(value) => return Ok(value),
            Err(_) if attempt < max_attempts => {}
            Err(err) => return Err(err),
        }
        tokio::time::sleep(base_delay * attempt).await;
    }
}

/// Send a request, retrying *transient* failures with linear backoff: transport
/// errors (timeouts, dropped connections), HTTP 429, and 5xx. Responses that are
/// not transient — 2xx and other 4xx (e.g. 401/403/400) — are returned as-is for
/// the caller to interpret, since retrying them would not help.
///
/// `build` is called once per attempt, so it must reconstruct the request (the
/// request is consumed by `send`).
///
/// A transport error is returned without its request URL, for every client: a URL can
/// carry a credential in its query (Pinboard's `auth_token`), and `reqwest::Error` would
/// otherwise print it in both `Display` and `Debug`. `label` names the endpoint instead.
pub async fn send_retrying<F>(
    label: &str,
    max_attempts: u32,
    base_delay: Duration,
    mut build: F,
) -> Result<reqwest::Response>
where
    F: FnMut() -> reqwest::RequestBuilder,
{
    retry(
        max_attempts,
        base_delay,
        |resp: &reqwest::Response| {
            resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS
                || resp.status().is_server_error()
        },
        || build().send(),
    )
    .await
    .map_err(reqwest::Error::without_url)
    .with_context(|| format!("{label} failed after {max_attempts} attempts"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    const NO_DELAY: Duration = Duration::from_millis(0);

    /// Retry over a canned sequence of outcomes (status code, or `Err`), counting
    /// how many attempts were made. `retry_on_ok` treats 429/5xx as transient.
    async fn run(outcomes: Vec<Result<u16, ()>>, max: u32) -> (Result<u16, ()>, usize) {
        let calls = RefCell::new(0usize);
        let seq = RefCell::new(outcomes.into_iter());
        let result = retry(
            max,
            NO_DELAY,
            |s: &u16| *s == 429 || (500..600).contains(s),
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
        let (result, calls) = run(vec![Err(()), Ok(200)], 5).await;
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
        let (result, calls) = run(vec![Err(()), Err(()), Err(())], 3).await;
        assert_eq!(result, Err(()));
        assert_eq!(calls, 3);
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
        let err = send_retrying("posts/all", 2, NO_DELAY, || {
            http.get("ftp://api.pinboard.in/v1/posts/all")
                .query(&[("auth_token", "user:SECRET123")])
        })
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

        let err = send_retrying("posts/all", 2, Duration::ZERO, || {
            http.get(&url).query(&[("auth_token", "user:SECRET123")])
        })
        .await
        .unwrap_err();

        assert_error_hides(&err, reqwest::Error::is_connect, "SECRET123");
    }
}
