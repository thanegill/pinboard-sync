use super::*;

const NO_DELAY: Duration = Duration::from_millis(0);
const NO_CONNECT_BACKOFF: [Duration; 3] = [NO_DELAY; 3];

/// A failed attempt: `Connect` never connected, `Timeout` got no response headers,
/// `Transport` failed after connecting (e.g. a dropped connection).
#[derive(Debug, PartialEq)]
enum Failure {
    Connect,
    Timeout,
    Transport,
}

impl Failure {
    fn reach(&self) -> Reach {
        match self {
            Failure::Connect => Reach::NoConnection,
            Failure::Timeout => Reach::NoAnswer,
            Failure::Transport => Reach::Reached,
        }
    }
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
    let (result, calls) =
        attempt_classified(budget, HOST, &NO_CONNECT_BACKOFF, outcomes, max, classify).await;
    (result.expect("the host was given up"), calls)
}

const HOST: &str = "host.test";

/// [`run_on`] against `host`, which may be given up: then `Err(Unreachable)`.
async fn attempt_on(
    budget: &ConnectBudget,
    host: &str,
    outcomes: Vec<Result<u16, Failure>>,
    max: u32,
) -> (Result<Result<u16, Failure>, Unreachable>, usize) {
    attempt_classified(
        budget,
        host,
        &NO_CONNECT_BACKOFF,
        outcomes,
        max,
        backoff_on_429_and_5xx,
    )
    .await
}

async fn attempt_classified(
    budget: &ConnectBudget,
    host: &str,
    connect_backoff: &[Duration],
    outcomes: Vec<Result<u16, Failure>>,
    max: u32,
    classify: fn(&u16) -> Verdict,
) -> (Result<Result<u16, Failure>, Unreachable>, usize) {
    let calls = RefCell::new(0usize);
    let seq = RefCell::new(outcomes.into_iter());
    let policy = RetryPolicy::new(
        budget.host(host),
        max,
        NO_DELAY,
        connect_backoff,
        classify,
        Failure::reach,
    );
    let result = retry_over(policy, || {
        *calls.borrow_mut() += 1;
        let next = seq.borrow_mut().next().expect("ran out of canned outcomes");
        async move { next }
    })
    .await;
    (result, calls.into_inner())
}

impl Resend for () {
    fn resend(&self) -> Option<Self> {
        Some(())
    }
}

/// Drives `policy` over a mock service whose every call is `op()`.
async fn retry_over<Fut>(
    policy: RetryPolicy<'_, u16, Failure>,
    op: impl Fn() -> Fut + Clone,
) -> Result<Result<u16, Failure>, Unreachable>
where
    Fut: Future<Output = Result<u16, Failure>>,
{
    retry(policy, tower::service_fn(move |()| op()), ()).await
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
    let budget = ConnectBudget::default();
    let policy = RetryPolicy::new(
        budget.host(HOST),
        5,
        Duration::from_secs(2),
        &NO_CONNECT_BACKOFF,
        |status: &u16| match *status {
            429 => Verdict::RetryOnceAfter(Duration::from_secs(90)),
            _ => Verdict::Done,
        },
        Failure::reach,
    );
    let result = retry_over(policy, || {
        attempt_offsets.borrow_mut().push(start.elapsed().as_secs());
        let status = statuses.borrow_mut().next().unwrap();
        async move { Ok(status) }
    })
    .await;

    assert_eq!(result, Ok(Ok(200)));
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
    let budget = ConnectBudget::default();
    let policy = RetryPolicy::new(
        budget.host(HOST),
        5,
        Duration::from_secs(3),
        &CONNECT_BACKOFF,
        |_: &u16| Verdict::Done,
        Failure::reach,
    );
    let result = retry_over(policy, || {
        attempt_offsets.borrow_mut().push(start.elapsed().as_secs());
        async { Err(Failure::Connect) }
    })
    .await;

    assert_eq!(result, Ok(Err(Failure::Connect)));
    assert_eq!(attempt_offsets.into_inner(), [0, 5, 15, 35, 75, 155]);
}

fn connect_failures(count: usize) -> Vec<Result<u16, Failure>> {
    (0..count).map(|_| Err(Failure::Connect)).collect()
}

async fn spend_backoff(budget: &ConnectBudget, host: &str) {
    let (result, calls) = attempt_on(budget, host, connect_failures(4), 2).await;
    assert_eq!(result, Ok(Err(Failure::Connect)));
    assert_eq!(
        calls, 4,
        "the first outage waits out the whole connect backoff"
    );
}

#[tokio::test]
async fn a_spent_connect_budget_falls_back_to_linear_retries() {
    let budget = ConnectBudget::default();
    spend_backoff(&budget, HOST).await;

    let (result, calls) = run_on(&budget, vec![Err(Failure::Connect), Ok(200)], 2).await;
    assert_eq!(result, Ok(200));
    assert_eq!(
        calls, 2,
        "a spent budget still retries a connect error linearly"
    );

    let (result, calls) = run_on(&budget, connect_failures(2), 2).await;
    assert_eq!(result, Err(Failure::Connect));
    assert_eq!(
        calls, 2,
        "later requests on the client get only max_attempts"
    );
}

#[tokio::test]
async fn a_host_that_fails_again_after_its_backoff_is_given_up() {
    let budget = ConnectBudget::default();
    let before = Outage::now().began_at;
    spend_backoff(&budget, HOST).await;

    let (result, calls) = attempt_on(&budget, HOST, connect_failures(2), 2).await;
    assert_eq!(result, Ok(Err(Failure::Connect)));
    assert_eq!(calls, 2);

    let (result, calls) = attempt_on(&budget, HOST, vec![Ok(200)], 2).await;
    assert_eq!(calls, 0, "a given-up host gets no attempt");
    let unreachable = result.unwrap_err();
    assert_eq!(unreachable.host, HOST);
    assert!(
        (before..=Outage::now().began_at).contains(&unreachable.since),
        "since is when the outage began: {unreachable:?}"
    );
}

#[tokio::test]
async fn a_host_is_given_up_only_when_its_last_attempt_fails_to_connect() {
    let budget = ConnectBudget::default();
    spend_backoff(&budget, HOST).await;

    let outcomes = vec![Err(Failure::Connect), Err(Failure::Transport)];
    let (result, _) = attempt_on(&budget, HOST, outcomes, 2).await;
    assert_eq!(result, Ok(Err(Failure::Transport)));
    let (result, _) = attempt_on(&budget, HOST, vec![Err(Failure::Connect), Ok(503)], 2).await;
    assert_eq!(result, Ok(Ok(503)));

    let (result, calls) = attempt_on(&budget, HOST, vec![Ok(200)], 2).await;
    assert_eq!((result, calls), (Ok(Ok(200)), 1));
}

#[tokio::test]
async fn the_outage_start_resets_once_the_host_answers() {
    let budget = ConnectBudget::default();
    spend_backoff(&budget, HOST).await;
    let outage_start = budget.host(HOST).state().failing_since;
    assert!(outage_start.is_some());

    let (result, _) = attempt_on(&budget, HOST, vec![Ok(200)], 2).await;
    assert_eq!(result, Ok(Ok(200)));
    assert_eq!(budget.host(HOST).state().failing_since, None);
}

#[tokio::test]
async fn each_host_has_its_own_backoff_and_is_given_up_alone() {
    let budget = ConnectBudget::default();
    let (site, search) = ("news.example", "search.example");
    spend_backoff(&budget, site).await;
    let (result, _) = attempt_on(&budget, site, connect_failures(2), 2).await;
    assert_eq!(result, Ok(Err(Failure::Connect)));
    let (result, calls) = attempt_on(&budget, site, vec![Ok(200)], 2).await;
    assert!(result.is_err() && calls == 0, "the site is given up");

    let (result, calls) =
        attempt_on(&budget, search, vec![Err(Failure::Connect), Ok(200)], 2).await;
    assert_eq!(result, Ok(Ok(200)));
    assert_eq!(calls, 2);
    spend_backoff(&budget, search).await;
}

const LONG_BACKOFF: [Duration; 2] = [Duration::from_secs(60); 2];

async fn attempt_long(
    budget: &ConnectBudget,
    outcomes: Vec<Result<u16, Failure>>,
) -> (Result<Result<u16, Failure>, Unreachable>, usize) {
    attempt_classified(
        budget,
        HOST,
        &LONG_BACKOFF,
        outcomes,
        2,
        backoff_on_429_and_5xx,
    )
    .await
}

#[tokio::test(start_paused = true)]
async fn an_outage_that_runs_on_past_the_backoff_is_given_up_by_the_next_request() {
    let budget = ConnectBudget::default();
    let (result, calls) = attempt_long(&budget, connect_failures(3)).await;
    assert_eq!((result, calls), (Ok(Err(Failure::Connect)), 3));
    let outage_began_at = at("2000-01-01T00:00:00Z");
    budget.host(HOST).update(|state| {
        state.failing_since.as_mut().unwrap().began_at = outage_began_at;
    });

    let (result, _) = attempt_long(&budget, connect_failures(2)).await;
    assert_eq!(result, Ok(Err(Failure::Connect)));

    let (result, calls) = attempt_long(&budget, vec![Ok(200)]).await;
    assert_eq!(calls, 0);
    assert_eq!(
        result,
        Err(Unreachable {
            host: HOST.into(),
            since: outage_began_at,
        })
    );
}

#[tokio::test(start_paused = true)]
async fn a_fresh_outage_is_given_up_only_once_it_outlasts_the_backoff() {
    let budget = ConnectBudget::default();
    let (result, _) = attempt_long(&budget, connect_failures(3)).await;
    assert_eq!(result, Ok(Err(Failure::Connect)));

    let (result, _) = attempt_long(&budget, vec![Ok(503), Err(Failure::Connect)]).await;
    assert_eq!(result, Ok(Err(Failure::Connect)));
    let (result, _) = attempt_long(&budget, connect_failures(2)).await;
    assert_eq!(
        result,
        Ok(Err(Failure::Connect)),
        "an outage that began after the 503 is too young to give up on"
    );

    tokio::time::advance(LONG_BACKOFF.iter().sum()).await;
    let (result, _) = attempt_long(&budget, connect_failures(2)).await;
    assert_eq!(result, Ok(Err(Failure::Connect)));
    let (result, calls) = attempt_long(&budget, vec![Ok(200)]).await;
    assert!(result.is_err() && calls == 0, "the host is given up");
}

fn timeouts(count: usize) -> Vec<Result<u16, Failure>> {
    (0..count).map(|_| Err(Failure::Timeout)).collect()
}

#[tokio::test(start_paused = true)]
async fn a_silent_host_is_given_up_once_its_timeouts_outlast_the_backoff() {
    let budget = ConnectBudget::default();
    let (result, calls) = attempt_long(&budget, timeouts(2)).await;
    assert_eq!((result, calls), (Ok(Err(Failure::Timeout)), 2));
    assert!(
        !budget.host(HOST).state().backoff_spent,
        "a timeout never takes the long connect wait"
    );

    tokio::time::advance(LONG_BACKOFF.iter().sum()).await;
    let (result, _) = attempt_long(&budget, vec![Ok(503), Err(Failure::Timeout)]).await;
    assert_eq!(
        result,
        Ok(Err(Failure::Timeout)),
        "a 5xx is a response, so the outage starts over"
    );
    assert!(budget.host(HOST).unreachable().is_none());

    tokio::time::advance(LONG_BACKOFF.iter().sum()).await;
    let (result, _) = attempt_long(&budget, timeouts(2)).await;
    assert_eq!(result, Ok(Err(Failure::Timeout)));
    let (result, calls) = attempt_long(&budget, vec![Ok(200)]).await;
    assert!(result.is_err() && calls == 0, "the host is given up");
}

#[tokio::test(start_paused = true)]
async fn one_request_whose_timeouts_outlast_the_backoff_gives_its_host_up() {
    let budget = ConnectBudget::default();
    let attempts = Cell::new(0);
    let policy = RetryPolicy::new(
        budget.host(HOST),
        2,
        NO_DELAY,
        &LONG_BACKOFF,
        backoff_on_429_and_5xx,
        Failure::reach,
    );
    let result = retry_over(policy, || {
        attempts.set(attempts.get() + 1);
        async {
            tokio::time::sleep(LONG_BACKOFF[0]).await;
            Err(Failure::Timeout)
        }
    })
    .await;
    assert_eq!((result, attempts.get()), (Ok(Err(Failure::Timeout)), 2));

    let (result, calls) = attempt_long(&budget, vec![Ok(200)]).await;
    assert!(
        result.is_err() && calls == 0,
        "two minute-long timeouts span the two-minute schedule from the first one's start"
    );
}

#[tokio::test(start_paused = true)]
async fn an_outage_starts_with_the_attempt_that_first_got_no_response() {
    let budget = ConnectBudget::default();
    let outcomes = RefCell::new(vec![Ok(503), Err(Failure::Timeout)].into_iter());
    let policy = RetryPolicy::new(
        budget.host(HOST),
        2,
        NO_DELAY,
        &LONG_BACKOFF,
        backoff_on_429_and_5xx,
        Failure::reach,
    );
    let result = retry_over(policy, || {
        let next = outcomes.borrow_mut().next().unwrap();
        async {
            tokio::time::sleep(LONG_BACKOFF[0]).await;
            next
        }
    })
    .await;
    assert_eq!(result, Ok(Err(Failure::Timeout)));

    let (result, calls) = attempt_long(&budget, vec![Ok(200)]).await;
    assert_eq!(
        (result, calls),
        (Ok(Ok(200)), 1),
        "one minute-long timeout after a 503 is too short an outage to give up on"
    );
}

#[tokio::test(start_paused = true)]
async fn connect_failures_and_timeouts_make_one_outage() {
    for second_request in [
        vec![Err(Failure::Timeout), Err(Failure::Connect)],
        vec![Err(Failure::Connect), Err(Failure::Timeout)],
    ] {
        let budget = ConnectBudget::default();
        let (result, _) = attempt_long(&budget, connect_failures(3)).await;
        assert_eq!(result, Ok(Err(Failure::Connect)));

        attempt_long(&budget, second_request)
            .await
            .0
            .unwrap()
            .unwrap_err();
        let (result, calls) = attempt_long(&budget, vec![Ok(200)]).await;
        assert!(
            result.is_err() && calls == 0,
            "the outage that spent the backoff ran on through the timeout"
        );
    }
}

#[test]
fn unreachable_names_the_host_and_when_it_went_down() {
    let unreachable = Unreachable {
        host: "hn.algolia.com".into(),
        since: at("2026-10-25T05:30:00Z"),
    };
    assert_eq!(
        unreachable.to_string(),
        "giving up: hn.algolia.com unreachable since 2026-10-25T05:30:00Z"
    );
}

#[tokio::test]
async fn a_connect_outage_that_clears_leaves_the_budget_unspent() {
    let budget = ConnectBudget::default();
    let (result, _) = run_on(&budget, vec![Err(Failure::Connect), Ok(200)], 2).await;
    assert_eq!(result, Ok(200));

    let (result, calls) = run_on(&budget, (0..4).map(|_| Err(Failure::Connect)).collect(), 2).await;
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

#[test]
fn every_client_gets_the_read_timeout() {
    let client = format!("{:?}", client_builder().build().unwrap());
    assert!(
        client.contains(&format!("read_timeout: {READ_TIMEOUT:?}")),
        "{client}"
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

#[tokio::test]
async fn a_request_that_cannot_be_built_fails_at_once_without_its_url() {
    let http = reqwest::Client::new();
    let builds = Cell::new(0);

    let err = send_with_connect_backoff(
        "posts/all",
        &ConnectBudget::default(),
        2,
        NO_DELAY,
        &NO_CONNECT_BACKOFF,
        backoff_if_transient,
        || {
            builds.set(builds.get() + 1);
            http.get("unix:/v1/posts/all?auth_token=user:SECRET123")
        },
    )
    .await
    .unwrap_err();

    assert_eq!(builds.get(), 1);
    assert!(
        format!("{err:#}").starts_with("building posts/all: "),
        "{err:#}"
    );
    assert_error_hides(&err, reqwest::Error::is_builder, "SECRET123");
}

#[test]
fn excerpt_is_collapsed_to_one_line() {
    assert_eq!(
        error_excerpt("\n  <h1>400</h1>\r\n\t<p>Bad Request</p>\n"),
        "<h1>400</h1> <p>Bad Request</p>"
    );
}

#[test]
fn excerpt_is_cut_on_a_char_boundary_within_the_cap() {
    // `ELLIPSIS` is 3 bytes, so the cut offset is odd and lands inside an `é` here.
    let body = format!("xx{}", "é".repeat(ERROR_BODY_MAX_BYTES));
    let excerpt = error_excerpt(&body);
    assert!(excerpt.len() <= ERROR_BODY_MAX_BYTES);
    let kept = excerpt.strip_suffix(ELLIPSIS).expect("cut is marked");
    assert_eq!(kept.len(), ERROR_BODY_MAX_BYTES - ELLIPSIS.len() - 1);
    assert!(body.starts_with(kept));
}

#[test]
fn excerpt_at_the_cap_is_kept_whole() {
    let body = "x".repeat(ERROR_BODY_MAX_BYTES);
    assert_eq!(error_excerpt(&body), body);
}

#[test]
fn error_with_body_quotes_the_excerpt_after_what() {
    let err = error_with_body("github repo returned 500", "  oops\n  try again  ", &[]);
    assert_eq!(err.to_string(), "github repo returned 500: oops try again");
}

#[test]
fn error_with_body_quotes_nothing_for_a_blank_body() {
    for blank in ["", " \n\t "] {
        let err = error_with_body("github repo returned 502", blank, &[]);
        assert_eq!(err.to_string(), "github repo returned 502", "{blank:?}");
    }
}

#[test]
fn redact_replaces_the_longest_secret_first_and_skips_an_empty_one() {
    assert_eq!(
        redact(
            "sent reddit_session=abc; a=1, then abc alone",
            &["", "abc", "reddit_session=abc; a=1"]
        ),
        "sent [redacted], then [redacted] alone"
    );
}

#[tokio::test]
async fn error_with_body_redacts_the_logged_body_and_the_excerpt() {
    let body = format!("bad token tok-123\n{}\ntok-123 again", "x".repeat(400));
    let (err, logs) = crate::test_support::capturing_logs(async {
        error_with_body("github repo returned 500", &body, &["tok-123"])
    })
    .await;
    let [debug] = logs.as_slice() else {
        panic!("expected the body at debug level, got {logs:?}");
    };
    assert!(debug.ends_with(r#"\n[redacted] again""#), "{debug}");
    assert!(!debug.contains("tok-123"), "{debug}");
    let message = err.to_string();
    assert!(
        message.starts_with("github repo returned 500: bad token [redacted] x"),
        "{message}"
    );
}

#[tokio::test]
async fn log_body_at_debug_redacts_the_body() {
    let ((), logs) = crate::test_support::capturing_logs(async {
        log_body_at_debug("req returned 403", "cookie abc echoed", &["abc"])
    })
    .await;
    assert_eq!(
        logs,
        [r#"DEBUG req returned 403; full response body: "cookie [redacted] echoed""#]
    );
}

#[test]
fn debug_line_keeps_the_whole_body_escaped_on_one_line() {
    let x = "x".repeat(2 * ERROR_BODY_MAX_BYTES);
    let body = format!("<p class=\"é\">\r\n{x}\nERROR forged entry\n</p>");
    let line = body_debug_line("github repo returned 500", &body);
    assert_eq!(
        line,
        format!(
            r#"github repo returned 500; full response body: "<p class=\"é\">\r\n{x}\nERROR forged entry\n</p>""#
        )
    );
    assert!(!line.contains(['\n', '\r']), "{line}");
}
