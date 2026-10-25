//! Shared HTTP helpers used by every client: the retrying send, and quoting a response
//! body in an error.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use time::format_description::well_known::Rfc2822;
use time::OffsetDateTime;
use tower::retry::{Policy, Retry};
use tower::ServiceExt;

/// How long one attempt may take to connect, from DNS through the TLS handshake.
/// hyper-util, under reqwest, splits the TCP part evenly across each address family's
/// resolved addresses. A connect that runs out of it is a connect error, so it is retried
/// on [`CONNECT_BACKOFF`].
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Bounds the wait from send to the response headers (connect included), then each gap
/// between body chunks; must outlast [`CONNECT_TIMEOUT`] so a slow connect still takes the
/// connect backoff.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

const _: () = assert!(READ_TIMEOUT.as_secs() > CONNECT_TIMEOUT.as_secs());

/// How long after its last frame an HTTP/2 connection is pinged, idle or not.
const H2_PING_INTERVAL: Duration = Duration::from_secs(30);

/// An unanswered ping this long closes the connection, failing its requests before
/// [`READ_TIMEOUT`] would, so the retry reconnects instead of reusing the dead one.
const H2_PING_TIMEOUT: Duration = Duration::from_secs(20);

const _: () =
    assert!(H2_PING_INTERVAL.as_secs() + H2_PING_TIMEOUT.as_secs() < READ_TIMEOUT.as_secs());

/// The waits before each retry of a request that could not establish a connection (a
/// DNS, TCP or TLS failure). Exponential and long, about 2.5 minutes of waiting in all,
/// so a resolver or network that is down for a minute or two is ridden out. The attempts
/// themselves add to that: against a host that drops SYNs each one blocks for
/// [`CONNECT_TIMEOUT`]. Not every connect failure clears (a bad certificate, a wrong base
/// URL); those also wait the schedule out before failing.
const CONNECT_BACKOFF: [Duration; 5] = [
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(20),
    Duration::from_secs(40),
    Duration::from_secs(80),
];

/// One client's connect allowance, per host: the long [`CONNECT_BACKOFF`] wait is spent
/// once, and a host with no response for longer than that schedule, unbroken, is given up.
/// Neither is undone within a run.
#[derive(Default)]
pub struct ConnectBudget {
    hosts: RefCell<HashMap<String, HostState>>,
}

impl ConnectBudget {
    fn host<'a>(&'a self, host: &'a str) -> HostBudget<'a> {
        HostBudget { budget: self, host }
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct HostState {
    backoff_spent: bool,
    /// The unbroken run of unanswered attempts, if any.
    failing_since: Option<Outage>,
    /// When the outage that gave the host up began.
    given_up: Option<OffsetDateTime>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reach {
    /// DNS, TCP or TLS failed.
    NoConnection,
    /// No response headers within [`READ_TIMEOUT`], or an unanswered HTTP/2 ping.
    NoAnswer,
    Reached,
}

impl Reach {
    fn of(err: &reqwest::Error) -> Reach {
        if err.is_connect() {
            Reach::NoConnection
        } else if err.is_timeout() {
            Reach::NoAnswer
        } else {
            Reach::Reached
        }
    }
}

/// Monotonic to measure, so a wall-clock step can't move a give-up; wall-clock to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Outage {
    began: tokio::time::Instant,
    began_at: OffsetDateTime,
}

impl Outage {
    fn now() -> Self {
        let now = OffsetDateTime::now_utc();
        Outage {
            began: tokio::time::Instant::now(),
            began_at: now.replace_nanosecond(0).unwrap_or(now),
        }
    }
}

#[derive(Clone, Copy)]
struct HostBudget<'a> {
    budget: &'a ConnectBudget,
    host: &'a str,
}

impl HostBudget<'_> {
    fn state(&self) -> HostState {
        self.budget
            .hosts
            .borrow()
            .get(self.host)
            .copied()
            .unwrap_or_default()
    }

    fn update(&self, change: impl FnOnce(&mut HostState)) {
        change(
            self.budget
                .hosts
                .borrow_mut()
                .entry(self.host.to_string())
                .or_default(),
        );
    }

    /// `started` is when the attempt began, so an outage dates from its first unanswered
    /// attempt.
    fn record_attempt(&self, started: Outage, unanswered: bool) {
        self.update(|state| {
            if unanswered {
                state.failing_since.get_or_insert(started);
            } else {
                state.failing_since = None;
            }
        });
    }

    fn outage_lasting(&self, span: Duration) -> Option<Outage> {
        self.state()
            .failing_since
            .filter(|outage| outage.began.elapsed() >= span)
    }

    fn unreachable(&self) -> Option<Unreachable> {
        self.state().given_up.map(|since| Unreachable {
            host: self.host.to_string(),
            since,
        })
    }
}

/// A request not sent because its host was given up.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Unreachable {
    host: String,
    since: OffsetDateTime,
}

impl fmt::Display for Unreachable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let since = crate::timefmt::to_rfc3339(self.since).unwrap_or_default();
        write!(f, "giving up: {} unreachable since {since}", self.host)
    }
}

impl std::error::Error for Unreachable {}

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

/// How tower's [`Retry`] retries one request to `host`: connect failures on
/// `connect_delays` while the host's long wait is unspent, other failures and
/// [`Verdict::Backoff`] responses linearly up to `max_attempts`, and a
/// [`Verdict::RetryOnceAfter`] once. Ending its linear attempts without a response, after
/// `give_up_after` of unbroken outage, gives the host up.
///
/// `Retry` clones the policy once per request, so its counters start fresh; `attempts` and
/// `attempt_started` are shared with that clone.
struct RetryPolicy<'a, Res, E> {
    host: HostBudget<'a>,
    max_attempts: u32,
    base_delay: Duration,
    connect_delays: std::slice::Iter<'a, Duration>,
    give_up_after: Duration,
    classify: Rc<dyn Fn(&Res) -> Verdict + 'a>,
    reach: fn(&E) -> Reach,
    other_attempts: u32,
    waited_once: bool,
    attempts: Rc<Cell<u32>>,
    attempt_started: Rc<Cell<Outage>>,
}

impl<Res, E> Clone for RetryPolicy<'_, Res, E> {
    fn clone(&self) -> Self {
        RetryPolicy {
            connect_delays: self.connect_delays.clone(),
            classify: Rc::clone(&self.classify),
            attempts: Rc::clone(&self.attempts),
            attempt_started: Rc::clone(&self.attempt_started),
            ..*self
        }
    }
}

impl<'a, Res, E> RetryPolicy<'a, Res, E> {
    /// Call just before sending: the first attempt's start is stamped now.
    fn new(
        host: HostBudget<'a>,
        max_attempts: u32,
        base_delay: Duration,
        connect_backoff: &'a [Duration],
        classify: impl Fn(&Res) -> Verdict + 'a,
        reach: fn(&E) -> Reach,
    ) -> Self {
        RetryPolicy {
            host,
            max_attempts,
            base_delay,
            connect_delays: connect_backoff.iter(),
            give_up_after: connect_backoff.iter().sum(),
            classify: Rc::new(classify),
            reach,
            other_attempts: 0,
            waited_once: false,
            attempts: Rc::default(),
            attempt_started: Rc::new(Cell::new(Outage::now())),
        }
    }

    /// How long to wait before trying again after `outcome`, or `None` to return it.
    fn delay_after(&mut self, outcome: &Result<Res, E>) -> Option<Duration> {
        let reached = match outcome {
            Ok(_) => Reach::Reached,
            Err(err) => (self.reach)(err),
        };
        self.host
            .record_attempt(self.attempt_started.get(), reached != Reach::Reached);
        if reached == Reach::NoConnection && !self.host.state().backoff_spent {
            let delay = self.connect_delays.next().copied();
            if delay.is_none() {
                self.host.update(|state| state.backoff_spent = true);
            }
            return delay;
        }
        let verdict = match outcome {
            Ok(value) => (self.classify)(value),
            Err(_) => Verdict::Backoff,
        };
        match verdict {
            Verdict::Done => None,
            Verdict::RetryOnceAfter(_) if self.waited_once => None,
            Verdict::RetryOnceAfter(wait) => {
                self.waited_once = true;
                Some(wait)
            }
            Verdict::Backoff => {
                self.other_attempts += 1;
                if self.other_attempts < self.max_attempts {
                    return Some(self.base_delay * self.other_attempts);
                }
                if let Some(outage) = self.host.outage_lasting(self.give_up_after) {
                    self.host
                        .update(|state| state.given_up = Some(outage.began_at));
                }
                None
            }
        }
    }
}

/// `None` silently stops retries and their count; reqwest returns it only for a streaming
/// body, which no request here has.
trait Resend: Sized {
    fn resend(&self) -> Option<Self>;
}

impl Resend for reqwest::Request {
    fn resend(&self) -> Option<Self> {
        self.try_clone()
    }
}

impl<Req: Resend, Res, E> Policy<Req, Res, E> for RetryPolicy<'_, Res, E> {
    type Future = Pin<Box<dyn Future<Output = ()>>>;

    fn retry(&mut self, _: &mut Req, outcome: &mut Result<Res, E>) -> Option<Self::Future> {
        self.attempts.set(self.attempts.get() + 1);
        let delay = self.delay_after(outcome)?;
        let attempt_started = Rc::clone(&self.attempt_started);
        Some(Box::pin(async move {
            sleep(delay).await;
            attempt_started.set(Outage::now());
        }))
    }

    fn clone_request(&mut self, request: &Req) -> Option<Req> {
        request.resend()
    }
}

/// `Err(Unreachable)`, without an attempt, if the policy's host is given up.
async fn retry<S, Req>(
    policy: RetryPolicy<'_, S::Response, S::Error>,
    service: S,
    request: Req,
) -> Result<Result<S::Response, S::Error>, Unreachable>
where
    S: tower::Service<Req> + Clone,
    Req: Resend,
{
    if let Some(unreachable) = policy.host.unreachable() {
        return Err(unreachable);
    }
    Ok(Retry::new(policy, service).oneshot(request).await)
}

#[cfg(test)]
tokio::task_local! {
    static RECORDED_WAITS: std::cell::RefCell<Vec<Duration>>;
}

/// Sleeps for `delay`, or, inside `recording_waits`, records it and returns at once.
pub(crate) async fn sleep(delay: Duration) {
    #[cfg(test)]
    if RECORDED_WAITS
        .try_with(|waits| waits.borrow_mut().push(delay))
        .is_ok()
    {
        return;
    }
    tokio::time::sleep(delay).await;
}

/// Runs `fut` with every wait recorded instead of slept, returning its output and
/// the waits in order. For a test that must know how long a request waited without
/// timing it, or that talks to a real socket and must not sleep: a paused clock
/// auto-advances to the next timer whenever the runtime idles on I/O.
#[cfg(test)]
pub(crate) async fn recording_waits<T>(fut: impl Future<Output = T>) -> (T, Vec<Duration>) {
    RECORDED_WAITS
        .scope(std::cell::RefCell::default(), async {
            let output = fut.await;
            (output, RECORDED_WAITS.with(std::cell::RefCell::take))
        })
        .await
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

/// The starting point for every client's `reqwest::Client`. The TLS ClientHello offers
/// ALPN `h2, http/1.1`, and the client speaks whichever the server picks. Reddit's edge
/// 403s a ClientHello with no ALPN extension (#78), which `native-tls-alpn` adds.
/// Connecting (DNS, TCP and the TLS handshake) is bounded by [`CONNECT_TIMEOUT`], and
/// silence by [`READ_TIMEOUT`]. No overall timeout: it would cap a slow but steady
/// `posts/all`.
pub fn client_builder() -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(READ_TIMEOUT)
        .http2_keep_alive_interval(H2_PING_INTERVAL)
        .http2_keep_alive_timeout(H2_PING_TIMEOUT)
        .http2_keep_alive_while_idle(true)
}

/// Send a request, retrying *transient* failures. A request that could not establish a
/// connection (per `reqwest::Error::is_connect`: any connector failure, so DNS, TCP
/// connect, or the TLS handshake, including one that runs past [`CONNECT_TIMEOUT`]) is
/// retried on the long exponential [`CONNECT_BACKOFF`] (5s, 10s, 20s, 40s, 80s), so a
/// short resolver or network outage doesn't fail the run, once per host per client (see
/// [`ConnectBudget`], which also gives up hosts). Other transport errors (a connection
/// dropped mid-request, or no response headers within [`READ_TIMEOUT`]), HTTP 429, and
/// 5xx are retried with linear backoff, `base_delay * n` for up to `max_attempts`
/// attempts. Responses that are not transient — 2xx and other 4xx (e.g. 401/403/400) —
/// are returned as-is for the caller to interpret, since retrying them would not help.
///
/// Only sending is retried: a body that stalls after this returns fails the caller's read.
///
/// `build` is called once and the request cloned for each attempt; one that cannot be built
/// fails at once as "building `label`".
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
    F: FnOnce() -> reqwest::RequestBuilder,
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
    F: FnOnce() -> reqwest::RequestBuilder,
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
    build: F,
) -> Result<reqwest::Response>
where
    F: FnOnce() -> reqwest::RequestBuilder,
{
    let (client, request) = build().build_split();
    let request = request
        .map_err(reqwest::Error::without_url)
        .with_context(|| format!("building {label}"))?;
    let host = authority(request.url());
    let policy = RetryPolicy::new(
        budget.host(&host),
        max_attempts,
        base_delay,
        connect_backoff,
        classify,
        Reach::of,
    );
    let attempts = Rc::clone(&policy.attempts);
    retry(policy, client, request)
        .await
        .with_context(|| format!("{label} not sent"))?
        .map_err(reqwest::Error::without_url)
        .with_context(|| format!("{label} failed after {} attempts", attempts.get()))
        .inspect(|resp| log::debug!("{}", protocol_debug_line(resp)))
}

/// The debug line naming the protocol a response came over, for checking what each
/// service negotiates. It names the host alone, since the URL can carry a credential in
/// its query (Pinboard's `auth_token`).
fn protocol_debug_line(resp: &reqwest::Response) -> String {
    format!(
        "{} answered over {:?}",
        resp.url().host_str().unwrap_or("an unnamed host"),
        resp.version()
    )
}

/// The host and any non-default port of `url`: what a [`ConnectBudget`] tracks.
fn authority(url: &reqwest::Url) -> String {
    let host = url.host_str().unwrap_or_default();
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

/// The longest [`error_excerpt`] returns, in bytes, including [`ELLIPSIS`].
pub const ERROR_BODY_MAX_BYTES: usize = 300;
/// Ends an [`error_excerpt`] that was cut.
pub const ELLIPSIS: &str = "…";

/// Stands in for a secret [`redact`] removed.
pub const REDACTED: &str = "[redacted]";

/// The error `"{what}: {excerpt}"`, quoting a response body as [`error_excerpt`] cuts it,
/// or just `"{what}"` when the body is blank, with [`body_debug_line`] logged at debug
/// level first so `-v` shows what the excerpt left out. Both carry the body with `secrets`
/// [redacted](redact) first, so the cut can't leave part of one behind.
pub fn error_with_body(what: &str, body: &str, secrets: &[&str]) -> anyhow::Error {
    let body = redact(body, secrets);
    log::debug!("{}", body_debug_line(what, &body));
    let excerpt = error_excerpt(&body);
    if excerpt.is_empty() {
        return anyhow!("{what}");
    }
    anyhow!("{what}: {excerpt}")
}

/// Logs [`body_debug_line`] at debug level with `secrets` [redacted](redact), for a body
/// worth seeing under `-v` whether or not an error quotes it.
pub fn log_body_at_debug(what: &str, body: &str, secrets: &[&str]) {
    log::debug!("{}", body_debug_line(what, &redact(body, secrets)));
}

/// `text` with every occurrence of each non-empty secret replaced by [`REDACTED`], the
/// longest first, so a secret that contains another (a whole cookie and its session
/// value) is replaced whole.
pub fn redact(text: &str, secrets: &[&str]) -> String {
    let mut longest_first = secrets.to_vec();
    longest_first.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    longest_first
        .into_iter()
        .filter(|secret| !secret.is_empty())
        .fold(text.to_string(), |text, secret| {
            text.replace(secret, REDACTED)
        })
}

/// The debug line [`log_body_at_debug`] logs: the whole body, uncut, as an escaped string
/// literal, so its newlines can't start what looks like a separate log line in the journal.
fn body_debug_line(what: &str, body: &str) -> String {
    format!("{what}; full response body: {body:?}")
}

/// `text` with its whitespace collapsed to one line and cut to at most
/// [`ERROR_BODY_MAX_BYTES`] on a char boundary, with [`ELLIPSIS`] marking the cut, so a
/// large HTML error page stays out of the journal.
fn error_excerpt(text: &str) -> String {
    let one_line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.len() <= ERROR_BODY_MAX_BYTES {
        return one_line;
    }
    let cut = one_line.floor_char_boundary(ERROR_BODY_MAX_BYTES - ELLIPSIS.len());
    format!("{}{ELLIPSIS}", &one_line[..cut])
}

#[cfg(test)]
mod tests {
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
}

/// Tests that need a socket; the flake skips `net_tests` in the Nix build sandbox.
#[cfg(test)]
mod net_tests {
    use super::tests::assert_error_hides;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

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
    async fn client_hello_offers_alpn_h2_and_http1() {
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
        assert_eq!(
            offered_alpn(&hello),
            Some(vec!["h2".to_string(), "http/1.1".to_string()])
        );
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

    // A paused clock with a real socket fires timers early, which is sound only because
    // this listener never answers: the timeout is the expected outcome either way.
    #[tokio::test(start_paused = true)]
    async fn a_stalled_connect_times_out_onto_the_connect_backoff() {
        let silent_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = silent_listener.local_addr().unwrap().port();
        let http = client_builder().no_proxy().build().unwrap();
        let start = tokio::time::Instant::now();

        let sent = tokio::time::timeout(
            CONNECT_TIMEOUT * 5,
            send_with_connect_backoff(
                "stalled",
                &ConnectBudget::default(),
                2,
                Duration::ZERO,
                &[Duration::ZERO; 3],
                backoff_if_transient,
                || http.get(format!("https://127.0.0.1:{port}/")),
            ),
        )
        .await
        .expect("the connect timeout never fired");

        let err = sent.unwrap_err();
        assert!(
            format!("{err:#}").contains("failed after 4 attempts"),
            "a timed-out connect must take the connect backoff: {err:#}"
        );
        assert!(
            err.downcast_ref::<reqwest::Error>()
                .is_some_and(|err| err.is_connect() && err.is_timeout()),
            "unexpected error: {err:#}"
        );
        assert_eq!(start.elapsed(), CONNECT_TIMEOUT * 4);
        drop(silent_listener);
    }

    /// What [`h2c_server`] does with each request it receives, in order, whichever
    /// connection it arrives on.
    #[derive(Clone, Copy)]
    enum H2Reply {
        Ok,
        /// Answer 200, then start a graceful shutdown (GOAWAY `NO_ERROR`), as a server
        /// retiring an idle or long-lived connection does.
        OkThenGoAway,
        Reset(h2::Reason),
        /// Shut the whole connection down instead of answering: a GOAWAY that counts
        /// this stream as processed, then the socket closes under it.
        Abort,
        /// Stop serving this connection, pings included, but keep its socket open.
        Hang,
        /// Answer 200, then [`H2Reply::Hang`] once the response has had time to go out.
        OkThenHang,
    }

    /// An HTTP/2 server without TLS on a local port, answering per `replies`, and a
    /// count of the TCP connections it has accepted. Without TLS there is no ALPN, so a
    /// client must speak h2 by prior knowledge; past the handshake hyper drives an
    /// ALPN-negotiated connection the same way.
    async fn h2c_server(replies: Vec<H2Reply>) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let connections = Arc::new(AtomicUsize::new(0));
        let accepted = Arc::clone(&connections);
        let replies = Arc::new(Mutex::new(replies.into_iter()));
        tokio::spawn(async move {
            loop {
                let (socket, _) = listener.accept().await.unwrap();
                accepted.fetch_add(1, Ordering::SeqCst);
                let replies = Arc::clone(&replies);
                tokio::spawn(async move {
                    let mut conn = h2::server::handshake(socket).await.unwrap();
                    while let Some(Ok((_, mut respond))) = conn.accept().await {
                        let reply = replies.lock().unwrap().next().expect("no reply left");
                        let ok = || http::Response::new(());
                        match reply {
                            H2Reply::Ok => drop(respond.send_response(ok(), true)),
                            H2Reply::OkThenGoAway => {
                                drop(respond.send_response(ok(), true));
                                conn.graceful_shutdown();
                            }
                            H2Reply::Reset(reason) => respond.send_reset(reason),
                            H2Reply::Abort => {
                                conn.abrupt_shutdown(h2::Reason::INTERNAL_ERROR);
                                let _ = std::future::poll_fn(|cx| conn.poll_closed(cx)).await;
                                return;
                            }
                            H2Reply::Hang => {
                                std::future::pending::<()>().await;
                            }
                            H2Reply::OkThenHang => {
                                drop(respond.send_response(ok(), true));
                                let flush = std::future::poll_fn(|cx| conn.poll_closed(cx));
                                let _ =
                                    tokio::time::timeout(Duration::from_millis(50), flush).await;
                                std::future::pending::<()>().await;
                            }
                        }
                    }
                });
            }
        });
        (addr, connections)
    }

    const H2_LINEAR_DELAY: Duration = Duration::from_secs(1);
    const H2_CONNECT_BACKOFF: [Duration; 3] = [Duration::from_secs(60); 3];

    /// One `send_retrying` GET to `url` over `http`, with the waits it took. The linear
    /// and connect schedules differ, so the waits show which one an error was given.
    async fn h2_get(
        http: &reqwest::Client,
        url: &str,
    ) -> (Result<reqwest::Response>, Vec<Duration>) {
        recording_waits(send_with_connect_backoff(
            "h2c",
            &ConnectBudget::default(),
            3,
            H2_LINEAR_DELAY,
            &H2_CONNECT_BACKOFF,
            backoff_if_transient,
            || http.get(url),
        ))
        .await
    }

    fn h2_client() -> reqwest::Client {
        client_builder()
            .no_proxy()
            .http2_prior_knowledge()
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn a_reset_h2_stream_is_retried_on_the_linear_backoff() {
        let (addr, connections) = h2c_server(vec![
            H2Reply::Reset(h2::Reason::INTERNAL_ERROR),
            H2Reply::Ok,
        ])
        .await;

        let (sent, waits) = h2_get(&h2_client(), &format!("http://{addr}/")).await;

        assert_eq!(sent.unwrap().version(), reqwest::Version::HTTP_2);
        assert_eq!(waits, [H2_LINEAR_DELAY]);
        assert_eq!(connections.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn each_response_logs_its_protocol_naming_the_host_alone() {
        let (addr, _) = h2c_server(vec![H2Reply::Ok]).await;
        let url = format!("http://{addr}/v1/posts/all?auth_token=user:SECRET123");

        let ((sent, _), logs) =
            crate::test_support::capturing_logs(h2_get(&h2_client(), &url)).await;

        assert_eq!(sent.unwrap().status(), 200);
        assert_eq!(logs, ["DEBUG 127.0.0.1 answered over HTTP/2.0"]);
    }

    #[tokio::test]
    async fn an_h2_connection_that_dies_mid_run_is_replaced_on_the_linear_backoff() {
        let (addr, connections) = h2c_server(vec![H2Reply::Ok, H2Reply::Abort, H2Reply::Ok]).await;
        let url = format!("http://{addr}/");
        let http = h2_client();

        let (sent, waits) = h2_get(&http, &url).await;
        assert_eq!(sent.unwrap().status(), 200);
        assert!(waits.is_empty(), "{waits:?}");

        let (sent, waits) = h2_get(&http, &url).await;
        assert_eq!(sent.unwrap().status(), 200);
        assert_eq!(
            waits,
            [H2_LINEAR_DELAY],
            "a request on a connection the server kills is not a connect error"
        );
        assert_eq!(connections.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_reset_stream_or_killed_h2_connection_does_not_count_against_its_host() {
        for reply in [H2Reply::Reset(h2::Reason::INTERNAL_ERROR), H2Reply::Abort] {
            let (addr, _) = h2c_server(vec![reply; 3]).await;
            let budget = ConnectBudget::default();

            let (sent, waits) = recording_waits(send_with_connect_backoff(
                "h2c",
                &budget,
                3,
                H2_LINEAR_DELAY,
                &H2_CONNECT_BACKOFF,
                backoff_if_transient,
                || h2_client().get(format!("http://{addr}/")),
            ))
            .await;

            assert!(sent.is_err());
            assert_eq!(waits, [H2_LINEAR_DELAY, H2_LINEAR_DELAY * 2]);
            let state = budget.host(&addr.to_string()).state();
            assert_eq!((state.failing_since, state.given_up), (None, None));
        }
    }

    #[tokio::test]
    async fn reqwest_itself_retries_a_refused_h2_stream() {
        let (addr, _) = h2c_server(vec![
            H2Reply::Reset(h2::Reason::REFUSED_STREAM),
            H2Reply::Ok,
        ])
        .await;

        let (sent, waits) = h2_get(&h2_client(), &format!("http://{addr}/")).await;

        assert_eq!(sent.unwrap().status(), 200);
        assert!(waits.is_empty(), "{waits:?}");
    }

    #[tokio::test]
    async fn a_connection_retired_between_requests_costs_no_retry() {
        let (addr, connections) = h2c_server(vec![H2Reply::OkThenGoAway, H2Reply::Ok]).await;
        let url = format!("http://{addr}/");
        let http = h2_client();

        for _ in 0..2 {
            let (sent, waits) = h2_get(&http, &url).await;
            assert_eq!(sent.unwrap().status(), 200);
            assert!(waits.is_empty(), "{waits:?}");
        }
        assert_eq!(connections.load(Ordering::SeqCst), 2);
    }

    /// [`h2_client`] with pings short enough to find a dead connection well within its
    /// one-second read timeout.
    fn h2_pinging_client() -> reqwest::Client {
        client_builder()
            .no_proxy()
            .http2_prior_knowledge()
            .read_timeout(Duration::from_secs(1))
            .http2_keep_alive_interval(Duration::from_millis(100))
            .http2_keep_alive_timeout(Duration::from_millis(100))
            .build()
            .unwrap()
    }

    #[tokio::test]
    async fn a_dead_h2_connection_is_found_by_its_pings_and_the_retry_reconnects() {
        let (addr, connections) = h2c_server(vec![H2Reply::Ok, H2Reply::Hang, H2Reply::Ok]).await;
        let url = format!("http://{addr}/");
        let http = h2_pinging_client();

        let (sent, _) = h2_get(&http, &url).await;
        assert_eq!(sent.unwrap().status(), 200);

        let (sent, waits) = h2_get(&http, &url).await;
        assert_eq!(sent.unwrap().status(), 200);
        assert_eq!(waits, [H2_LINEAR_DELAY]);
        assert_eq!(connections.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_idle_dead_h2_connection_is_dropped_before_the_next_request() {
        let (addr, connections) = h2c_server(vec![H2Reply::OkThenHang, H2Reply::Ok]).await;
        let url = format!("http://{addr}/");
        let http = h2_pinging_client();

        let (sent, _) = h2_get(&http, &url).await;
        assert_eq!(sent.unwrap().status(), 200);
        tokio::time::sleep(Duration::from_millis(500)).await;

        let (sent, waits) = h2_get(&http, &url).await;
        assert_eq!(sent.unwrap().status(), 200);
        assert!(waits.is_empty(), "{waits:?}");
        assert_eq!(connections.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn an_unanswered_h2_ping_counts_as_no_answer_until_a_new_connection_answers() {
        let (addr, _) = h2c_server(vec![H2Reply::Ok, H2Reply::Hang, H2Reply::Ok]).await;
        let url = format!("http://{addr}/");
        let http = h2_pinging_client();
        let budget = ConnectBudget::default();
        let send = || {
            recording_waits(send_with_connect_backoff(
                "h2c",
                &budget,
                1,
                H2_LINEAR_DELAY,
                &H2_CONNECT_BACKOFF,
                backoff_if_transient,
                || http.get(&url),
            ))
        };

        assert_eq!(send().await.0.unwrap().status(), 200);
        let err = send().await.0.unwrap_err();

        assert!(
            err.downcast_ref::<reqwest::Error>()
                .is_some_and(|err| err.is_timeout() && !err.is_connect()),
            "a ping timeout is a timeout, not a connect failure: {err:#}"
        );
        let host = addr.to_string();
        assert!(budget.host(&host).state().failing_since.is_some());

        assert_eq!(send().await.0.unwrap().status(), 200);
        let state = budget.host(&host).state();
        assert_eq!((state.failing_since, state.given_up), (None, None));
    }

    #[tokio::test]
    async fn a_given_up_host_is_not_contacted_again() {
        let port = {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let http = short_read_timeout_client();
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
            "{err:#}"
        );
        let err = send().await.unwrap_err();
        assert!(
            format!("{err:#}").contains("failed after 2 attempts"),
            "{err:#}"
        );

        let revived = std::net::TcpListener::bind(("127.0.0.1", port)).unwrap();
        revived.set_nonblocking(true).unwrap();
        let err = send().await.unwrap_err();
        let message = format!("{err:#}");
        assert!(
            message.starts_with(&format!(
                "posts/all not sent: giving up: 127.0.0.1:{port} unreachable since "
            )),
            "{message}"
        );
        assert!(!format!("{err:?}").contains("SECRET123"), "{err:?}");
        assert_eq!(
            revived.accept().map_err(|err| err.kind()).unwrap_err(),
            std::io::ErrorKind::WouldBlock,
            "the third request must not connect, though the port now listens"
        );
    }

    #[tokio::test]
    async fn a_silent_host_is_given_up_and_not_contacted_again() {
        let silent_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        silent_listener.set_nonblocking(true).unwrap();
        let port = silent_listener.local_addr().unwrap().port();
        let connections_made = || std::iter::from_fn(|| silent_listener.accept().ok()).count();
        let http = short_read_timeout_client();
        let budget = ConnectBudget::default();
        let send = || {
            send_with_connect_backoff(
                "silent",
                &budget,
                2,
                Duration::ZERO,
                &[Duration::ZERO; 3],
                backoff_if_transient,
                || http.get(format!("http://127.0.0.1:{port}/")),
            )
        };

        let err = send().await.unwrap_err();
        assert!(
            format!("{err:#}").contains("failed after 2 attempts"),
            "{err:#}"
        );
        assert_eq!(connections_made(), 2);

        let err = send().await.unwrap_err();
        assert!(
            format!("{err:#}").starts_with(&format!(
                "silent not sent: giving up: 127.0.0.1:{port} unreachable since "
            )),
            "{err:#}"
        );
        assert_eq!(connections_made(), 0, "a given-up host gets no attempt");
    }

    #[tokio::test]
    async fn a_body_that_stalls_does_not_count_against_its_host() {
        use std::io::Write;

        let (release, held) = std::sync::mpsc::channel::<()>();
        let port = serve_one(move |mut socket| {
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial")
                .unwrap();
            let _ = held.recv();
        });
        let http = short_read_timeout_client();
        let budget = ConnectBudget::default();

        let resp = send_with_connect_backoff(
            "stalling",
            &budget,
            2,
            Duration::ZERO,
            &[Duration::ZERO; 3],
            backoff_if_transient,
            || http.get(format!("http://127.0.0.1:{port}/")),
        )
        .await
        .unwrap();
        assert!(resp.text().await.unwrap_err().is_timeout());

        let host = format!("127.0.0.1:{port}");
        let state = budget.host(&host).state();
        assert_eq!(
            (state.failing_since, state.given_up),
            (None, None),
            "the host answered, so it is not failing"
        );
        drop(release);
    }

    const SHORT_READ_TIMEOUT: Duration = Duration::from_millis(300);

    fn short_read_timeout_client() -> reqwest::Client {
        client_builder()
            .no_proxy()
            .read_timeout(SHORT_READ_TIMEOUT)
            .build()
            .unwrap()
    }

    /// Reads the request head, then hands the socket to `respond`.
    fn serve_one(respond: impl FnOnce(std::net::TcpStream) + Send + 'static) -> u16 {
        use std::io::Read;

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket.set_nodelay(true).unwrap();
            let mut head = Vec::new();
            let mut byte = [0u8];
            while !head.ends_with(b"\r\n\r\n") {
                socket.read_exact(&mut byte).unwrap();
                head.push(byte[0]);
            }
            respond(socket);
        });
        port
    }

    #[tokio::test]
    async fn a_silent_server_times_out_onto_the_linear_retries() {
        let silent_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = silent_listener.local_addr().unwrap().port();
        let http = short_read_timeout_client();

        let sent = tokio::time::timeout(
            Duration::from_secs(10),
            send_with_connect_backoff(
                "silent",
                &ConnectBudget::default(),
                2,
                Duration::ZERO,
                &[Duration::ZERO; 3],
                backoff_if_transient,
                || http.get(format!("http://127.0.0.1:{port}/")),
            ),
        )
        .await
        .expect("the read timeout never fired");

        let err = sent.unwrap_err();
        assert!(
            format!("{err:#}").contains("failed after 2 attempts"),
            "a read timeout must take the linear retries, not the connect backoff: {err:#}"
        );
        assert!(
            err.downcast_ref::<reqwest::Error>()
                .is_some_and(|err| err.is_timeout() && !err.is_connect()),
            "unexpected error: {err:#}"
        );
        drop(silent_listener);
    }

    #[tokio::test]
    async fn a_body_that_stalls_times_out_without_its_url() {
        use std::io::Write;

        let (release, held) = std::sync::mpsc::channel::<()>();
        let port = serve_one(move |mut socket| {
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\npartial")
                .unwrap();
            let _ = held.recv();
        });
        let http = short_read_timeout_client();
        let url = format!("http://127.0.0.1:{port}/v1/posts/all?auth_token=user:SECRET123");

        let read = tokio::time::timeout(Duration::from_secs(10), async {
            http.get(&url).send().await.unwrap().text().await
        })
        .await
        .expect("the read timeout never fired");

        let err = read.unwrap_err();
        assert!(err.is_timeout(), "unexpected error: {err:?}");
        for rendering in [format!("{err}"), format!("{err:?}")] {
            assert!(
                !rendering.contains("SECRET123"),
                "token leaked: {rendering}"
            );
        }
        drop(release);
    }

    #[tokio::test]
    async fn a_slow_body_that_keeps_arriving_is_not_cut_off() {
        use std::io::Write;

        let chunks = ["one ", "two ", "three ", "four ", "five ", "six"];
        let port = serve_one(move |mut socket| {
            let body_len = chunks.concat().len();
            write!(
                socket,
                "HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\n\r\n"
            )
            .unwrap();
            for chunk in chunks {
                std::thread::sleep(SHORT_READ_TIMEOUT / 4);
                socket.write_all(chunk.as_bytes()).unwrap();
            }
        });
        let http = short_read_timeout_client();
        let start = std::time::Instant::now();

        let body = http
            .get(format!("http://127.0.0.1:{port}/"))
            .send()
            .await
            .unwrap()
            .text()
            .await
            .unwrap();

        assert_eq!(body, chunks.concat());
        assert!(
            start.elapsed() > SHORT_READ_TIMEOUT,
            "the body must take longer than one read timeout in all"
        );
    }
}
