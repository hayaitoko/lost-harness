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

/// How long the serving thread sleeps between non-blocking `accept` attempts.
/// Short, because the sleep is time during which the listener holds no lock —
/// but correctness does not depend on it: see [`OneShotServer::requests`].
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(1);

/// Placeholder recorded the instant a connection is ACCEPTED — before a single
/// byte is read — and overwritten with the real bytes once the request has been
/// drained.
///
/// This exists so that "was this server contacted?" is answered by the
/// CONNECTION, not by the request body. Recording only after the drain meant a
/// contact could sit invisible for the whole 300 ms quiet period, and
/// `assert!(decoy.requests().is_empty())` in `loop_tests` — the branch's most
/// important negative assertion — could pass against a decoy that had already
/// been connected to.
const CONNECTION_ACCEPTED: &str = "<connection accepted; request not yet drained>";

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
    /// Shared with the serving thread, and kept here so the ASSERTING thread
    /// can drain the kernel's accept queue itself — see
    /// [`OneShotServer::requests`].
    listener: Arc<TcpListener>,
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
        let listener = Arc::new(listener);
        let serving = Arc::clone(&listener);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&requests);
        std::thread::spawn(move || {
            let deadline = Instant::now() + ACCEPT_TIMEOUT;
            loop {
                // `accept` and the record of it happen under ONE hold of the
                // sink lock. That is the whole race fix: without it there is an
                // instant where a connection has left the kernel's accept queue
                // but has not yet been written down, and an asserting thread
                // that samples exactly then sees neither — it finds nothing
                // pending on the listener AND an empty sink, and concludes the
                // server was never contacted.
                //
                // The old code was far worse than that instant: it recorded only
                // after the drain loop below timed out, so a live connection was
                // invisible for the whole 300 ms quiet period. `loop_tests`
                // asserts `decoy.requests().is_empty()` immediately after
                // `process_message` returns, so a decoy contacted concurrently
                // could false-pass the branch's most important negative test.
                let accepted = {
                    let mut sink = sink.lock().expect("request sink");
                    match serving.accept() {
                        Ok((sock, _)) => {
                            sink.push(CONNECTION_ACCEPTED.to_string());
                            Some((sock, sink.len() - 1))
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => None,
                        Err(_) => return,
                    }
                };
                let Some((mut sock, idx)) = accepted else {
                    if Instant::now() >= deadline {
                        return;
                    }
                    std::thread::sleep(ACCEPT_POLL_INTERVAL);
                    continue;
                };
                // `accept` on a non-blocking listener can hand back a
                // non-blocking socket on some platforms; make the conversation
                // blocking-with-timeout explicitly.
                let _ = sock.set_nonblocking(false);
                let _ = sock.set_read_timeout(Some(DRAIN_QUIET_PERIOD));
                let mut received = Vec::new();
                let mut buf = [0u8; 8192];
                // Drain until the peer goes quiet: the request body can straddle
                // several reads, and leaving it in the kernel buffer risks
                // stalling the client's write.
                while received.len() < MAX_CAPTURED_REQUEST {
                    match sock.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => received.extend_from_slice(&buf[..n]),
                        Err(_) => break, // timeout = the request is complete
                    }
                }
                // Upgrade the placeholder in place. Ordering matters: the
                // response goes out only AFTER this, so any client that got a
                // reply is guaranteed to see the real bytes, never the
                // placeholder.
                sink.lock().expect("request sink")[idx] =
                    String::from_utf8_lossy(&received).into_owned();
                let _ = sock.write_all(raw.as_bytes());
                let _ = sock.flush();
                return;
            }
        });
        Self {
            port,
            listener,
            requests,
        }
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

    /// Every contact this server has received, newest last. Empty means the
    /// server was never contacted — and that is a claim about CONNECTIONS now,
    /// not about drained request bodies.
    ///
    /// Race-free by construction, in two halves that only work together:
    ///  1. the serving thread accepts and records under one hold of the sink
    ///     lock, so a connection is never *taken* from the kernel queue without
    ///     being written down; and
    ///  2. this call holds that same lock while draining the queue itself, so a
    ///     connection that finished its handshake but that the serving thread
    ///     has not picked up yet is still counted.
    ///
    /// Together: at the instant this acquires the lock, every connection whose
    /// handshake had completed is either already in the sink or still in the
    /// backlog for us to find. There is no third state, and therefore no window
    /// in which a contacted server reports "never contacted".
    ///
    /// What remains — and no design can close it — is that this reports contacts
    /// that have already happened. A connection opened after the call obviously
    /// is not in it.
    pub fn requests(&self) -> Vec<String> {
        let mut sink = self.requests.lock().expect("request sink");
        // Anything sitting in the kernel's accept queue is a contact the serving
        // thread simply has not reached yet. Take it ourselves and record it —
        // dropping the socket is fine, since a test only reaches here to assert
        // about contacts, and a connection nobody is serving is one that should
        // never have been made.
        while let Ok((sock, _)) = self.listener.accept() {
            drop(sock);
            sink.push(CONNECTION_ACCEPTED.to_string());
        }
        sink.clone()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpStream;

    /// The negative assertion the endpoint-routing tests rest on
    /// (`assert!(decoy.requests().is_empty())`) must be about CONNECTIONS.
    ///
    /// The serving thread used to push into the sink only after its drain loop
    /// exited on the 300 ms quiet-period timeout, so a live connection was
    /// invisible for that whole window — and `loop_tests` asserts immediately
    /// after `process_message` returns. A decoy contacted concurrently could
    /// therefore false-pass the branch's most important test.
    #[test]
    fn a_bare_connection_is_recorded_at_once_with_no_bytes_sent() {
        let server = OneShotServer::spawn("HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        assert!(
            server.requests().is_empty(),
            "nothing has connected yet, so this must be empty"
        );

        // Connect and send NOTHING. `connect` returns once the handshake is
        // done, which is the moment the endpoint was "contacted".
        let _sock = TcpStream::connect(("127.0.0.1", server.port())).expect("connect");

        assert!(
            !server.requests().is_empty(),
            "a completed connection must be visible immediately, not after the \
             {DRAIN_QUIET_PERIOD:?} drain timeout"
        );
    }

    /// The accept-time placeholder must be replaced by the real bytes before
    /// the client can observe a response — otherwise the positive assertions
    /// (`first_request_line()`) would race the negative fix.
    #[test]
    fn a_served_request_still_records_its_actual_bytes() {
        let server = OneShotServer::spawn(
            "HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let mut sock = TcpStream::connect(("127.0.0.1", server.port())).expect("connect");
        sock.write_all(b"GET /models HTTP/1.1\r\nHost: x\r\n\r\n")
            .expect("write request");

        // Read to EOF. The server writes its response only after it has
        // overwritten the placeholder, so seeing a reply proves the ordering.
        let mut response = String::new();
        sock.read_to_string(&mut response).expect("read response");
        assert!(response.starts_with("HTTP/1.1 200 OK"), "got: {response:?}");

        let requests = server.requests();
        assert_eq!(
            requests.len(),
            1,
            "one connection, one record: {requests:?}"
        );
        assert_eq!(
            server.first_request_line().as_deref(),
            Some("GET /models HTTP/1.1")
        );
    }
}
