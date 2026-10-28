use super::tests::assert_error_hides;
use super::*;
use crate::test_support::strings;
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
    assert_eq!(offered_alpn(&hello), Some(strings(&["h2", "http/1.1"])));
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
                            let _ = tokio::time::timeout(Duration::from_millis(50), flush).await;
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

/// One GET to `url` over `http`, retried as `RetryingClient::send` retries, with the waits
/// it took. The linear and connect schedules differ, so the waits show which one an error
/// was given.
async fn h2_get(http: &reqwest::Client, url: &str) -> (Result<reqwest::Response>, Vec<Duration>) {
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

    let ((sent, _), logs) = crate::test_support::capturing_logs(h2_get(&h2_client(), &url)).await;

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

        sent.unwrap_err();
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
    client_builder_with_read_timeout(SHORT_READ_TIMEOUT)
        .no_proxy()
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
