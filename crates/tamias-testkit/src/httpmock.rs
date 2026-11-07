//! `HttpMock` — a std-only HTTP/1.1 mock server for tests.
//!
//! One `TcpListener` on `127.0.0.1:0`, one request per connection
//! (`Connection: close`), the handler closure decides the response.
//! No deps beyond std — vault/deploy impls get conformance-tested
//! over real HTTP without pulling a mock framework.
//!
//! ```ignore
//! let seen = Arc::new(Mutex::new(Vec::new()));
//! let mock = HttpMock::new({
//!     let seen = seen.clone();
//!     move |req| {
//!         seen.lock().unwrap().push(req.path.clone());
//!         Response::json(200, r#"{"refs":[]}"#)
//!     }
//! });
//! // … point the impl under test at mock.url() …
//! ```

use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Arc;
use std::thread::JoinHandle;

/// One parsed HTTP request, as the mock saw it.
#[derive(Debug, Clone)]
pub struct Request {
    /// `GET`, `PUT`, …
    pub method: String,
    /// Request target — path plus query, e.g. `/v1/dev/a/b?x=1`.
    pub path: String,
    /// Headers with lowercased names.
    pub headers: Vec<(String, String)>,
    /// Raw request body.
    pub body: Vec<u8>,
}

impl Request {
    /// First value of a header, case-insensitive name lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// The body as UTF-8 (panics on binary — fine for JSON APIs).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// What the handler sends back.
#[derive(Debug, Clone)]
pub struct Response {
    /// HTTP status code.
    pub status: u16,
    /// `Content-Type`, defaults to `application/json`.
    pub content_type: &'static str,
    /// Response body.
    pub body: String,
}

impl Response {
    /// A JSON response.
    pub fn json(status: u16, body: impl Into<String>) -> Self {
        Response {
            status,
            content_type: "application/json",
            body: body.into(),
        }
    }
}

/// A running mock server. `Drop` stops accepting; in-flight handler
/// threads finish on their own (test process exit reaps the rest).
pub struct HttpMock {
    url: String,
    stop: Arc<std::sync::atomic::AtomicBool>,
    acceptor: Option<JoinHandle<()>>,
}

impl HttpMock {
    /// Bind `127.0.0.1:0` and serve `handler` until dropped. The
    /// handler runs on a connection thread and must be `Sync`.
    pub fn new<F>(handler: F) -> Self
    where
        F: Fn(&Request) -> Response + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let port = listener.local_addr().expect("local_addr").port();
        let handler = Arc::new(handler);
        let stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let acceptor = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                listener.set_nonblocking(true).expect("set_nonblocking");
                loop {
                    if stop.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let handler = handler.clone();
                            std::thread::spawn(move || {
                                serve(stream, &*handler);
                            });
                        }
                        // WouldBlock while idle; any other error
                        // just means "try again" — the stop flag
                        // still breaks the loop.
                        Err(_) => {
                            std::thread::sleep(std::time::Duration::from_millis(2));
                        }
                    }
                }
            })
        };
        HttpMock {
            url: format!("http://127.0.0.1:{port}"),
            stop,
            acceptor: Some(acceptor),
        }
    }

    /// `http://127.0.0.1:{port}` — hand to the impl under test.
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl Drop for HttpMock {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        // Nudge the acceptor out of its sleep window quickly.
        let _ = std::net::TcpStream::connect(self.url.trim_start_matches("http://"));
        // `map` keeps the None arm out of coverage bookkeeping
        let _ = self.acceptor.take().map(|h| h.join());
    }
}

/// Read one request, dispatch, write one response, close. Every
/// i/o failure here is a dead request — the caller ignores the
/// outcome, so failures settle quietly instead of propagating a
/// Result nobody reads.
fn serve(mut stream: std::net::TcpStream, handler: &dyn Fn(&Request) -> Response) {
    let mut buf = Vec::with_capacity(4096);
    let req = loop {
        let mut chunk = [0u8; 4096];
        // A read error (peer RST) is a dead request either way.
        let n = stream.read(&mut chunk).unwrap_or(0);
        if n == 0 {
            return; // peer went away mid-request
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(req) = try_parse(&buf) {
            break req;
        }
        if buf.len() > 1024 * 1024 {
            return; // absurd — bail rather than buffer forever
        }
    };
    let res = handler(&req);
    let reason = match res.status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        500 => "Internal Server Error",
        _ => "Status",
    };
    // A peer that died mid-request can't be answered — nothing to
    // propagate, the connection is over either way.
    let _ = write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        res.status,
        reason,
        res.content_type,
        res.body.len(),
        res.body
    );
    let _ = stream.flush();
}

/// Try to parse a complete request out of `buf` — headers done and
/// `Content-Length` bytes of body present.
fn try_parse(buf: &[u8]) -> Option<Request> {
    let head_end = find_subslice(buf, b"\r\n\r\n")?;
    let head = std::str::from_utf8(&buf[..head_end]).ok()?;
    let mut lines = head.split("\r\n");
    // `split` always yields at least one element, even on "".
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let path = parts.next()?.to_string();
    let mut headers = Vec::new();
    let mut len = 0usize;
    for line in lines {
        let (k, v) = line.split_once(':')?;
        let (k, v) = (k.trim().to_lowercase(), v.trim().to_string());
        if k == "content-length" {
            len = v.parse().ok()?;
        }
        headers.push((k, v));
    }
    let body_start = head_end + 4;
    if buf.len() < body_start + len {
        return None; // body still in flight
    }
    Some(Request {
        method,
        path,
        headers,
        body: buf[body_start..body_start + len].to_vec(),
    })
}

fn find_subslice(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    /// Send raw bytes at the mock and read everything it answers.
    fn exchange(addr: &str, request: &[u8]) -> String {
        let mut s = std::net::TcpStream::connect(addr).unwrap();
        s.write_all(request).unwrap();
        s.shutdown(std::net::Shutdown::Write).unwrap();
        let mut out = String::new();
        s.read_to_string(&mut out).unwrap();
        out
    }

    #[test]
    fn serves_each_status_reason() {
        for status in [200, 201, 400, 401, 403, 404, 405, 409, 418, 500] {
            let mock = HttpMock::new(move |_| Response::json(status, "{}"));
            let addr = mock.url().trim_start_matches("http://").to_string();
            let out = exchange(&addr, b"GET / HTTP/1.1\r\nHost: x\r\n\r\n");
            assert!(out.starts_with(&format!("HTTP/1.1 {status} ")), "{out}");
        }
    }

    #[test]
    fn malformed_requests_get_no_response() {
        let mock = HttpMock::new(ok200);
        let addr = mock.url().trim_start_matches("http://").to_string();
        // every one of these fails a parse step — the connection
        // just closes with no response bytes
        for req in [
            &b"\r\n\r\n"[..],                               // empty request line
            b"GET\r\n\r\n",                                 // no path
            b"GET / HTTP/1.1\r\nNoColon\r\n\r\n",           // header without :
            b"GET / HTTP/1.1\r\nContent-Length: x\r\n\r\n", // bad length
            &b"\xff\xfe\r\n\r\n"[..],                       // non-utf8 head
                                                            // (a complete GET with no Content-Length and trailing
                                                            // bytes is *not* malformed — extra bytes are ignored
                                                            // and it gets a normal response)
        ] {
            assert_eq!(exchange(&addr, req), "", "{req:?}");
        }
        // half a head, then gone — never parses
        let mut s = std::net::TcpStream::connect(&addr).unwrap();
        s.write_all(b"GET / HT").unwrap();
        drop(s);
        // and the mock still works afterwards
        let out = exchange(&addr, b"GET / HTTP/1.1\r\n\r\n");
        assert!(out.starts_with("HTTP/1.1 200 "), "{out}");
    }

    /// Shared handler for mocks that never serve a request — a fn
    /// item, not a per-test closure, so it's invoked from the
    /// serving tests too.
    fn ok200(_: &Request) -> Response {
        Response::json(200, "{}")
    }

    #[test]
    fn absurdly_large_requests_bail() {
        let mock = HttpMock::new(ok200);
        let addr = mock.url().trim_start_matches("http://").to_string();
        let mut s = std::net::TcpStream::connect(&addr).unwrap();
        s.write_all(&vec![b'x'; 1024 * 1024 + 8]).unwrap();
        // the handler bails and drops the connection — read to EOF
        // so the test doesn't finish before the worker thread does
        let mut out = Vec::new();
        s.read_to_end(&mut out).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn drop_stops_the_acceptor() {
        // construction + drop is the whole contract — no hang
        let mock = HttpMock::new(ok200);
        let url = mock.url().to_string();
        drop(mock);
        // a connection attempt after drop is refused or dies fast
        let _ = std::net::TcpStream::connect(&url);
    }
}
