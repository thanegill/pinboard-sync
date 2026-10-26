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
/// [`RetryingClient::send_with`] decides it.
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

/// The default classifier, which [`RetryingClient::send`] uses: a 429 or 5xx is transient
/// and backs off; anything else goes back to the caller.
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

/// A client's `reqwest::Client` together with how its requests are retried: the linear
/// backoff's attempts and base delay, and its [`ConnectBudget`]. Every request a service
/// client sends goes through [`RetryingClient::send`], so the retry settings are chosen
/// once, where the client is built, rather than at each call.
pub struct RetryingClient {
    client: reqwest::Client,
    budget: ConnectBudget,
    max_attempts: u32,
    base_delay: Duration,
}

impl RetryingClient {
    pub fn new(client: reqwest::Client, max_attempts: u32, base_delay: Duration) -> Self {
        RetryingClient {
            client,
            budget: ConnectBudget::default(),
            max_attempts,
            base_delay,
        }
    }

    /// A GET request on the underlying client, for `build` to start from.
    pub fn get(&self, url: impl reqwest::IntoUrl) -> reqwest::RequestBuilder {
        self.client.get(url)
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
    /// Only sending is retried: a body that stalls after this returns fails the caller's
    /// read.
    ///
    /// `build` is called once and the request cloned for each attempt; one that cannot be
    /// built fails at once as "building `label`".
    ///
    /// A transport error is returned without its request URL, for every client: a URL can
    /// carry a credential in its query (Pinboard's `auth_token`), and `reqwest::Error` would
    /// otherwise print it in both `Display` and `Debug`. `label` names the endpoint instead.
    pub async fn send(
        &self,
        label: &str,
        build: impl FnOnce() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        self.send_with(label, backoff_if_transient, build).await
    }

    /// [`RetryingClient::send`], with `classify` deciding how each response is retried
    /// instead of [`backoff_if_transient`]. A response it calls [`Verdict::Done`] comes back
    /// as-is, for the caller to report.
    pub async fn send_with(
        &self,
        label: &str,
        classify: impl Fn(&reqwest::Response) -> Verdict,
        build: impl FnOnce() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        send_with_connect_backoff(
            label,
            &self.budget,
            self.max_attempts,
            self.base_delay,
            &CONNECT_BACKOFF,
            classify,
            build,
        )
        .await
    }
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
    let one_line = crate::htmltext::collapse_whitespace(text, " ");
    if one_line.len() <= ERROR_BODY_MAX_BYTES {
        return one_line;
    }
    let cut = one_line.floor_char_boundary(ERROR_BODY_MAX_BYTES - ELLIPSIS.len());
    format!("{}{ELLIPSIS}", &one_line[..cut])
}

#[cfg(test)]
mod tests;

/// Tests that need a socket; the flake skips `net_tests` in the Nix build sandbox.
#[cfg(test)]
mod net_tests;
