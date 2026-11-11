//! The HTTP layer every client shares: the retrying send, and quoting a body in an error.

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

/// The crate name and version. GitHub requires a User-Agent, and Reddit rejects a generic
/// or missing one.
pub const USER_AGENT: &str = concat!(env!("CARGO_PKG_NAME"), "/", env!("CARGO_PKG_VERSION"));

/// One attempt's connect, DNS through the TLS handshake. Running out is a connect error, so
/// it is retried on [`CONNECT_BACKOFF`].
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// The wait for the response headers (connect included), then for each body chunk. Longer
/// than [`CONNECT_TIMEOUT`], so a slow connect still takes the connect backoff.
const READ_TIMEOUT: Duration = Duration::from_secs(60);

const _: () = assert!(READ_TIMEOUT.as_secs() > CONNECT_TIMEOUT.as_secs());

/// How long after its last frame an HTTP/2 connection is pinged, idle or not.
const H2_PING_INTERVAL: Duration = Duration::from_secs(30);

/// An unanswered ping this long closes the connection, failing its requests before
/// [`READ_TIMEOUT`] would, so the retry reconnects instead of reusing the dead one.
const H2_PING_TIMEOUT: Duration = Duration::from_secs(20);

const _: () =
    assert!(H2_PING_INTERVAL.as_secs() + H2_PING_TIMEOUT.as_secs() < READ_TIMEOUT.as_secs());

/// The waits before each retry of a request that could not connect (DNS, TCP or TLS), about
/// 2.5 minutes in all, so a resolver or network down for a minute or two is ridden out.
/// Each attempt can add [`CONNECT_TIMEOUT`] against a host that drops SYNs, and a failure
/// that never clears (a bad certificate, a wrong base URL) waits it all out too.
const CONNECT_BACKOFF: [Duration; 5] = [
    Duration::from_secs(5),
    Duration::from_secs(10),
    Duration::from_secs(20),
    Duration::from_secs(40),
    Duration::from_secs(80),
];

/// The retry schedule for a transport error, 429 or 5xx: `attempts` tries in all, waiting
/// `delay * n` before the nth retry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LinearBackoff {
    attempts: u32,
    delay: Duration,
}

impl LinearBackoff {
    pub const fn new(attempts: u32, delay: Duration) -> Self {
        Self { attempts, delay }
    }
}

/// 4 tries, 2s, 4s and 6s apart: about 12s, enough for a 5xx blip, and given up long before
/// any rate limit resets.
pub const DEFAULT_BACKOFF: LinearBackoff = LinearBackoff::new(4, Duration::from_secs(2));

/// One client's connect allowance per host: the [`CONNECT_BACKOFF`] wait is spent once, and
/// a host with no response for longer than that schedule, unbroken, is given up for the
/// rest of the run.
#[derive(Default)]
pub struct ConnectBudget {
    hosts: RefCell<HashMap<String, HostState>>,
}

impl ConnectBudget {
    const fn host<'a>(&'a self, host: &'a str) -> HostBudget<'a> {
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
    fn of(err: &reqwest::Error) -> Self {
        if err.is_connect() {
            Self::NoConnection
        } else if err.is_timeout() {
            Self::NoAnswer
        } else {
            Self::Reached
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
        Self {
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
                .entry(self.host.to_owned())
                .or_default(),
        );
    }

    /// `started` is when the attempt began: an outage dates from its first unanswered one.
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
            host: self.host.to_owned(),
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

/// What the retry loop does with a response, as a classifier decides.
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
/// [`Verdict::Backoff`] on `linear`, and [`Verdict::RetryOnceAfter`] once. Running out of
/// linear attempts after `give_up_after` of unbroken outage gives the host up.
///
/// `Retry` clones the policy once per request, so `attempts` and `attempt_started` are
/// shared with the clone.
struct RetryPolicy<'a, Res, E> {
    host: HostBudget<'a>,
    linear: LinearBackoff,
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
        linear: LinearBackoff,
        connect_backoff: &'a [Duration],
        classify: impl Fn(&Res) -> Verdict + 'a,
        reach: fn(&E) -> Reach,
    ) -> Self {
        RetryPolicy {
            host,
            linear,
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
                if self.other_attempts < self.linear.attempts {
                    return Some(self.linear.delay * self.other_attempts);
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

/// `None` silently stops the retries and their count; reqwest returns it only for a
/// streaming body, which no request here has.
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
pub async fn sleep(delay: Duration) {
    #[cfg(test)]
    if RECORDED_WAITS
        .try_with(|waits| waits.borrow_mut().push(delay))
        .is_ok()
    {
        return;
    }
    tokio::time::sleep(delay).await;
}

/// Runs `fut` with every wait recorded instead of slept, returning its output and the waits
/// in order. For a test against a real socket, where a paused clock would auto-advance
/// whenever the runtime idles on I/O.
#[cfg(test)]
pub async fn recording_waits<T>(fut: impl Future<Output = T>) -> (T, Vec<Duration>) {
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

/// [`RetryingClient::send`]'s classifier: a 429 or 5xx backs off, anything else returns.
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

/// A classifier that retries a 429 once, after its `Retry-After`, when that is at most
/// `max_wait`; any other 429 returns. Other statuses as [`backoff_if_transient`].
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

/// Every client's `reqwest::Client` starts here. Its ClientHello offers ALPN
/// `h2, http/1.1`, since Reddit's edge 403s one without ALPN (#78). No overall timeout: it
/// would cap a slow but steady `posts/all`.
pub fn client_builder() -> reqwest::ClientBuilder {
    client_builder_with_read_timeout(READ_TIMEOUT)
}

/// [`client_builder`] with `read_timeout` in place of [`READ_TIMEOUT`], for tests.
#[expect(
    clippy::disallowed_methods,
    reason = "the one place a client's builder is made"
)]
pub fn client_builder_with_read_timeout(read_timeout: Duration) -> reqwest::ClientBuilder {
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(read_timeout)
        .http2_keep_alive_interval(H2_PING_INTERVAL)
        .http2_keep_alive_timeout(H2_PING_TIMEOUT)
        .http2_keep_alive_while_idle(true)
}

/// A `reqwest::Client` with its retry settings, a [`LinearBackoff`] and a
/// [`ConnectBudget`], chosen once where the client is built for every request it sends.
pub struct RetryingClient {
    client: reqwest::Client,
    budget: ConnectBudget,
    linear: LinearBackoff,
}

impl RetryingClient {
    pub fn new(client: reqwest::Client, linear: LinearBackoff) -> Self {
        Self {
            client,
            budget: ConnectBudget::default(),
            linear,
        }
    }

    /// A GET request on the underlying client, for `build` to start from.
    pub fn get(&self, url: impl reqwest::IntoUrl) -> reqwest::RequestBuilder {
        self.client.get(url)
    }

    /// Send a request, retrying transient failures. A connect failure (`is_connect`: DNS,
    /// TCP or TLS, past [`CONNECT_TIMEOUT`] included) waits out [`CONNECT_BACKOFF`] once
    /// per host ([`ConnectBudget`]); another transport error, a 429 or a 5xx retries on the
    /// client's [`LinearBackoff`]. Any other response is returned as-is. Only sending is
    /// retried: a body that stalls afterwards fails the caller's read.
    ///
    /// `build` is called once and the request cloned per attempt. A transport error drops
    /// its request URL, which can carry a credential (Pinboard's `auth_token`) that
    /// `reqwest::Error` prints; `label` names the endpoint instead.
    pub async fn send(
        &self,
        label: &str,
        build: impl FnOnce() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        self.send_with(label, backoff_if_transient, build).await
    }

    /// [`RetryingClient::send`], with `classify` in place of [`backoff_if_transient`].
    pub async fn send_with(
        &self,
        label: &str,
        classify: impl Fn(&reqwest::Response) -> Verdict,
        build: impl FnOnce() -> reqwest::RequestBuilder,
    ) -> Result<reqwest::Response> {
        send_with_connect_backoff(
            label,
            &self.budget,
            self.linear,
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
    linear: LinearBackoff,
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
        linear,
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
        .map(|mut resp| {
            resp.extensions_mut().insert(Attempts(attempts.get()));
            resp
        })
        .inspect(|resp| log::debug!("{}", protocol_debug_line(resp)))
}

/// How many attempts a [`RetryingClient::send`] response took, in its extensions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Attempts(u32);

/// Whether `resp` answered a retry, so an earlier attempt may already have taken effect.
pub fn answered_a_retry(resp: &reqwest::Response) -> bool {
    resp.extensions()
        .get::<Attempts>()
        .is_some_and(|Attempts(attempts)| *attempts > 1)
}

/// The debug line naming the protocol a response came over. It names the host alone: the
/// URL can carry a credential (Pinboard's `auth_token`).
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
    url.port()
        .map_or_else(|| host.to_owned(), |port| format!("{host}:{port}"))
}

/// The longest [`error_excerpt`] returns, in bytes, including [`ELLIPSIS`].
pub const ERROR_BODY_MAX_BYTES: usize = 300;
/// Ends a text that was cut: an [`error_excerpt`] or a [`crate::preview::excerpt`].
pub const ELLIPSIS: &str = "…";

/// Stands in for a secret [`redact`] removed.
pub const REDACTED: &str = "[redacted]";

/// `resp`'s body. When the caller acts on it (`decides`), a read cut off partway is an
/// error under `context`, without the URL, since a partial body could pass for another
/// answer; otherwise it only feeds an error excerpt, and a failed read leaves it empty.
pub async fn read_body(
    resp: reqwest::Response,
    decides: bool,
    context: impl FnOnce() -> String,
) -> Result<String> {
    match resp.text().await {
        Ok(body) => Ok(body),
        Err(read_error) if decides => Err(read_error.without_url()).with_context(context),
        Err(_) => Ok(String::new()),
    }
}

/// The error `"{what}: {excerpt}"` (just `"{what}"` for a blank body), with the whole body
/// logged at debug level for `-v`. `secrets` are [redacted](redact) before the body is cut
/// or logged, so the cut can't leave part of one behind.
pub fn error_with_body(what: &str, body: &str, secrets: &[&str]) -> anyhow::Error {
    let body = redact(body, secrets);
    log::debug!("{}", body_debug_line(what, &body));
    let excerpt = error_excerpt(&body);
    if excerpt.is_empty() {
        return anyhow!("{what}");
    }
    anyhow!("{what}: {excerpt}")
}

/// Logs a body at debug level with `secrets` [redacted](redact), for one worth seeing under
/// `-v` that no error quotes.
pub fn log_body_at_debug(what: &str, body: &str, secrets: &[&str]) {
    log::debug!("{}", body_debug_line(what, &redact(body, secrets)));
}

/// `text` with each non-empty secret replaced by [`REDACTED`], longest first, so one that
/// contains another (a cookie and its session value) goes whole.
pub fn redact(text: &str, secrets: &[&str]) -> String {
    let mut longest_first = secrets.to_vec();
    longest_first.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
    longest_first
        .into_iter()
        .filter(|secret| !secret.is_empty())
        .fold(text.to_owned(), |text, secret| {
            text.replace(secret, REDACTED)
        })
}

/// The whole body as an escaped string literal, so its newlines can't fake a separate log
/// line in the journal.
fn body_debug_line(what: &str, body: &str) -> String {
    format!("{what}; full response body: {body:?}")
}

/// `text` on one line, cut on a char boundary to [`ERROR_BODY_MAX_BYTES`] with
/// [`ELLIPSIS`], so a large HTML error page stays out of the journal.
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
