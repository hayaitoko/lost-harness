//! Shared `#[cfg(test)]` fakes.
//!
//! Anything a *second* module needs from another module's test block belongs
//! here rather than being copy-pasted — a copied fake drifts, and then two
//! tests that look identical stop testing the same thing.
//!
//! Today that is the loopback HTTP one-shot server: `ipc`'s redirect test and
//! `agent::loop_tests`' endpoint-routing tests both need a real socket on
//! `127.0.0.1` that a real `reqwest` client will actually connect to.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a one-shot server waits for its single connection before giving
/// up and letting its thread exit.
///
/// Bounded on purpose: several tests exist precisely to prove a server is
/// *never* contacted, and an unbounded `accept()` would park a thread for the
/// life of the test binary every time one of them runs.
const ACCEPT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long to keep draining one connection after the last byte arrives.
/// The client holds the socket open waiting for our response, so there is no
/// EOF to read towards — we drain until the peer goes quiet.
const DRAIN_QUIET_PERIOD: Duration = Duration::from_millis(300);

/// Cap on how much of one request we buffer. Far beyond any request a test
/// issues; exists so a runaway client can't grow this without bound.
const MAX_CAPTURED_REQUEST: usize = 256 * 1024;

/// A one-shot HTTP server on an ephemeral loopback port: it answers the FIRST
/// connection with a canned response, **records the bytes it received**, and
/// stops.
///
/// The recording half is the load-bearing part. A test can assert not only
/// that the endpoint it selected was contacted, but that the endpoint it did
/// *not* select saw nothing at all — which is the only way to prove "no
/// silent fallback to a different provider" against real HTTP rather than
/// against an injected stub that never opens a socket.
pub struct OneShotServer {
    port: u16,
    requests: Arc<Mutex<Vec<String>>>,
}

impl OneShotServer {
    /// Bind an ephemeral loopback port and serve `raw` (a complete HTTP
    /// response, headers included) to the first client.
    pub fn spawn(raw: impl Into<String>) -> Self {
        let raw = raw.into();
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
        let port = listener.local_addr().expect("local addr").port();
        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&requests);
        std::thread::spawn(move || {
            let deadline = Instant::now() + ACCEPT_TIMEOUT;
            loop {
                match listener.accept() {
                    Ok((mut sock, _)) => {
                        // `accept` on a non-blocking listener can hand back a
                        // non-blocking socket on some platforms; make the
                        // conversation blocking-with-timeout explicitly.
                        let _ = sock.set_nonblocking(false);
                        let _ = sock.set_read_timeout(Some(DRAIN_QUIET_PERIOD));
                        let mut received = Vec::new();
                        let mut buf = [0u8; 8192];
                        // Drain until the peer goes quiet: the request body
                        // can straddle several reads, and leaving it in the
                        // kernel buffer risks stalling the client's write.
                        while received.len() < MAX_CAPTURED_REQUEST {
                            match sock.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => received.extend_from_slice(&buf[..n]),
                                Err(_) => break, // timeout = the request is complete
                            }
                        }
                        sink.lock()
                            .expect("request sink")
                            .push(String::from_utf8_lossy(&received).into_owned());
                        let _ = sock.write_all(raw.as_bytes());
                        let _ = sock.flush();
                        return;
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(_) => return,
                }
            }
        });
        Self { port, requests }
    }

    /// The bound loopback port.
    pub fn port(&self) -> u16 {
        self.port
    }

    /// A `base_url` a `Provider` can be built from. Loopback, so
    /// `is_private_endpoint` treats it as on-device.
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }

    /// Every request this server has received, newest last. Empty means the
    /// server was never contacted.
    pub fn requests(&self) -> Vec<String> {
        self.requests.lock().expect("request sink").clone()
    }

    /// The request line (`POST /chat/completions HTTP/1.1`) of the first
    /// request, or `None` if nothing arrived.
    pub fn first_request_line(&self) -> Option<String> {
        self.requests()
            .first()
            .and_then(|r| r.lines().next().map(str::to_string))
    }
}

/// One-shot server that answers with `raw`, returning only its port — for
/// tests that don't need the recording.
pub fn one_shot_server(raw: &'static str) -> u16 {
    OneShotServer::spawn(raw).port()
}

/// A one-shot server that answers `302 Found` pointing at `port`. Separate
/// from [`one_shot_server`] because the `Location` header is built at runtime.
pub fn one_shot_server_redirecting_to(port: u16) -> u16 {
    OneShotServer::spawn(format!(
        "HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:{port}/models\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    ))
    .port()
}

/// A complete `200 OK` response carrying an OpenAI-compatible SSE
/// chat-completion stream that emits `text` and then the `[DONE]` sentinel.
///
/// No `Content-Length`: the body is framed by `Connection: close`, and the
/// socket closes when the serving thread returns.
pub fn sse_chat_response(text: &str) -> String {
    let body = format!(
        "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{text}\"}}}}]}}\n\
         data: [DONE]\n"
    );
    format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n{body}")
}
