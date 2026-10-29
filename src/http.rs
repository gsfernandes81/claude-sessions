//! Just enough HTTP/1.1 for the mail server and its client (`mail.rs`), on `std` alone.
//!
//! **Small on purpose, because the server reads untrusted input.** One request a connection
//! (`Connection: close` both ways), no chunked bodies, no continuation lines, a body only by
//! `Content-Length`, and a cap on the whole request; anything outside that is refused with a
//! status rather than guessed at. The client speaks the same subset to the same server.

use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream, ToSocketAddrs};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

/// The most a request may be, headers and body together.
pub const MAX_REQUEST: usize = 64 * 1024;
/// How long a client has to send its whole request.
pub const READ_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub method: String,
    /// The path without its query.
    pub path: String,
    pub query: Vec<(String, String)>,
    /// Names lowercased.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| *k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn param(&self, key: &str) -> Option<&str> {
        self.query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

impl Response {
    pub fn json(status: u16, body: String) -> Response {
        Response {
            status,
            body: body.into_bytes(),
        }
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Content Too Large",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

/// `%XX` and `+` undone; `None` for a broken escape or bytes that are not UTF-8.
pub fn unescape(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' => {
                let hex = s.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// Everything but the unreserved characters as `%XX`.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for &c in s.as_bytes() {
        if c.is_ascii_alphanumeric() || b"-._~".contains(&c) {
            out.push(c as char);
        } else {
            out.push_str(&format!("%{c:02X}"));
        }
    }
    out
}

fn split_target(target: &str) -> Option<(String, Vec<(String, String)>)> {
    let (path, q) = target.split_once('?').unwrap_or((target, ""));
    let mut query = Vec::new();
    for pair in q.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        query.push((unescape(k)?, unescape(v)?));
    }
    Some((unescape(path)?, query))
}

/// Where the headers end in `buf`: the index of the first body byte.
fn head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4)
}

/// One request from `r`, which must have arrived whole within `deadline` and within `max`
/// bytes. The error is the status to answer with and why.
pub fn read_request(
    r: &mut dyn Read,
    max: usize,
    deadline: Instant,
) -> Result<Request, (u16, String)> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let too_long = || (413, format!("a request is at most {max} bytes"));
    let (head, body_at) = loop {
        if let Some(end) = head_end(&buf) {
            break (buf[..end - 4].to_vec(), end);
        }
        if buf.len() > max {
            return Err(too_long());
        }
        if Instant::now() >= deadline {
            return Err((408, "the request did not arrive in time".into()));
        }
        match r.read(&mut chunk) {
            Ok(0) => return Err((400, "the request ended before its headers did".into())),
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                return Err((408, "the request did not arrive in time".into()));
            }
            Err(e) => return Err((400, format!("reading the request: {e}"))),
        }
    };
    let head = String::from_utf8(head).map_err(|_| (400, "headers are not UTF-8".to_string()))?;
    let mut lines = head.split("\r\n");
    let line = lines.next().unwrap_or("");
    let mut parts = line.split(' ');
    let (Some(method), Some(target), Some(version), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err((400, "the request line is not METHOD TARGET VERSION".into()));
    };
    if !version.starts_with("HTTP/1.") {
        return Err((400, format!("{version:?} is not HTTP/1")));
    }
    let Some((path, query)) = split_target(target) else {
        return Err((400, "the target is not escaped properly".into()));
    };
    let mut headers = Vec::new();
    for l in lines {
        let Some((k, v)) = l.split_once(':') else {
            return Err((400, "a header line has no colon".into()));
        };
        if k.is_empty() || k.contains(|c: char| c.is_whitespace()) {
            return Err((400, "a header name is not one".into()));
        }
        headers.push((k.to_ascii_lowercase(), v.trim().to_string()));
    }
    let mut req = Request {
        method: method.to_string(),
        path,
        query,
        headers,
        body: Vec::new(),
    };
    if req.header("transfer-encoding").is_some() {
        return Err((400, "only a Content-Length body is read".into()));
    }
    let len: usize = match req.header("content-length") {
        None => 0,
        Some(v) => v
            .parse()
            .map_err(|_| (400, "Content-Length is not a number".to_string()))?,
    };
    if body_at + len > max {
        return Err(too_long());
    }
    let mut body = buf[body_at..].to_vec();
    while body.len() < len {
        if Instant::now() >= deadline {
            return Err((408, "the request did not arrive in time".into()));
        }
        match r.read(&mut chunk) {
            Ok(0) => return Err((400, "the body is shorter than its Content-Length".into())),
            Ok(n) => body.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => return Err((408, "the request did not arrive in time".into())),
        }
    }
    body.truncate(len);
    req.body = body;
    Ok(req)
}

pub fn write_response(w: &mut dyn Write, resp: &Response) -> io::Result<()> {
    write!(
        w,
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\
         Connection: close\r\n\r\n",
        resp.status,
        reason(resp.status),
        resp.body.len()
    )?;
    w.write_all(&resp.body)?;
    w.flush()
}

/// A counting gate: at most `n` holders at once, the next one waiting for a place.
struct Gate {
    held: Mutex<usize>,
    freed: Condvar,
    n: usize,
}

impl Gate {
    fn enter(&self) {
        let mut held = self.held.lock().unwrap_or_else(|p| p.into_inner());
        while *held >= self.n {
            held = self.freed.wait(held).unwrap_or_else(|p| p.into_inner());
        }
        *held += 1;
    }

    fn leave(&self) {
        let mut held = self.held.lock().unwrap_or_else(|p| p.into_inner());
        *held -= 1;
        self.freed.notify_one();
    }
}

pub type Handler = Arc<dyn Fn(Request) -> Response + Send + Sync>;

/// Serve `listener` for ever: each connection on a thread of its own, at most `at_once` of
/// them, one request each. A request that cannot be read is answered with why.
pub fn serve(listener: TcpListener, at_once: usize, handler: Handler) -> io::Result<()> {
    let gate = Arc::new(Gate {
        held: Mutex::new(0),
        freed: Condvar::new(),
        n: at_once.max(1),
    });
    for stream in listener.incoming() {
        let Ok(mut stream) = stream else { continue };
        gate.enter();
        let gate = gate.clone();
        let handler = handler.clone();
        std::thread::spawn(move || {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let _ = stream.set_write_timeout(Some(Duration::from_secs(10)));
            let resp = match read_request(&mut stream, MAX_REQUEST, Instant::now() + READ_DEADLINE)
            {
                Ok(req) => handler(req),
                Err((status, why)) => Response::json(status, error_body(&why)),
            };
            let _ = write_response(&mut stream, &resp);
            let _ = stream.shutdown(std::net::Shutdown::Both);
            gate.leave();
        });
    }
    Ok(())
}

/// `{"error":"…"}`.
pub fn error_body(why: &str) -> String {
    let mut v = crate::json::Value::obj();
    v.set("error", crate::json::Value::string(why));
    crate::json::to_line(&v)
}

/// `http://host:port` (a trailing `/` allowed; port 80 when none) as host and port.
pub fn parse_url(url: &str) -> Result<(String, u16), String> {
    let rest = url
        .strip_prefix("http://")
        .ok_or_else(|| format!("{url:?} is not an http:// address"))?;
    let rest = rest.trim_end_matches('/');
    if rest.is_empty() || rest.contains('/') {
        return Err(format!("{url:?} is not http://host:port"));
    }
    match rest.rsplit_once(':') {
        Some((host, port)) => Ok((
            host.to_string(),
            port.parse()
                .map_err(|_| format!("{url:?} has no port number"))?,
        )),
        None => Ok((rest.to_string(), 80)),
    }
}

/// One request to `url`: the status and the body. `timeout` bounds the connect, and each read
/// and write; the lookup of the host's name is the system's and is not bounded here, which is
/// why the hook runs this on a thread it can leave behind.
pub fn call(
    url: &str,
    method: &str,
    target: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
) -> io::Result<(u16, Vec<u8>)> {
    let (host, port) =
        parse_url(url).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let addrs: Vec<_> = (host.as_str(), port).to_socket_addrs()?.collect();
    let mut last = io::Error::new(io::ErrorKind::NotFound, format!("{host} has no address"));
    let mut stream = None;
    for a in addrs {
        match TcpStream::connect_timeout(&a, timeout) {
            Ok(s) => {
                stream = Some(s);
                break;
            }
            Err(e) => last = e,
        }
    }
    let mut stream = stream.ok_or(last)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let mut head = format!(
        "{method} {target} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\
         Content-Length: {}\r\n",
        body.len()
    );
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    stream.flush()?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf)?;
    parse_response(&buf)
}

fn parse_response(buf: &[u8]) -> io::Result<(u16, Vec<u8>)> {
    let bad = |why: &str| io::Error::new(io::ErrorKind::InvalidData, why.to_string());
    let end = head_end(buf).ok_or_else(|| bad("the answer has no end to its headers"))?;
    let head = std::str::from_utf8(&buf[..end - 4]).map_err(|_| bad("headers not UTF-8"))?;
    let status: u16 = head
        .split("\r\n")
        .next()
        .and_then(|l| l.split(' ').nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("the answer has no status"))?;
    let len = head.split("\r\n").skip(1).find_map(|l| {
        let (k, v) = l.split_once(':')?;
        k.eq_ignore_ascii_case("content-length")
            .then(|| v.trim().parse::<usize>().ok())
            .flatten()
    });
    let mut body = buf[end..].to_vec();
    if let Some(len) = len {
        if body.len() < len {
            return Err(bad("the answer is shorter than its Content-Length"));
        }
        body.truncate(len);
    }
    Ok((status, body))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(raw: &[u8]) -> Result<Request, (u16, String)> {
        read_request(
            &mut io::Cursor::new(raw.to_vec()),
            MAX_REQUEST,
            Instant::now() + Duration::from_secs(5),
        )
    }

    #[test]
    fn a_request_is_read_with_its_query_headers_and_body() {
        let r = read(
            b"POST /send?a=1&b=x%20y&c HTTP/1.1\r\nHost: dev\r\nAuthorization: Bearer t\r\n\
              Content-Length: 5\r\n\r\nhello",
        )
        .unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.path, "/send");
        assert_eq!(r.param("a"), Some("1"));
        assert_eq!(r.param("b"), Some("x y"));
        assert_eq!(r.param("c"), Some(""));
        assert_eq!(r.header("AUTHORIZATION"), Some("Bearer t"));
        assert_eq!(r.body, b"hello");
    }

    #[test]
    fn what_is_not_the_subset_is_refused_with_a_status() {
        assert_eq!(read(b"GET / HTTP/1.1\r\nHost: x\r\n").unwrap_err().0, 400);
        assert_eq!(read(b"GET /\r\n\r\n").unwrap_err().0, 400);
        assert_eq!(read(b"GET / SPDY/3\r\n\r\n").unwrap_err().0, 400);
        assert_eq!(read(b"GET /%zz HTTP/1.1\r\n\r\n").unwrap_err().0, 400);
        assert_eq!(
            read(b"GET / HTTP/1.1\r\nno colon\r\n\r\n").unwrap_err().0,
            400
        );
        assert_eq!(
            read(b"GET / HTTP/1.1\r\nbad name: x\r\n\r\n")
                .unwrap_err()
                .0,
            400
        );
        assert_eq!(
            read(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n")
                .unwrap_err()
                .0,
            400
        );
        assert_eq!(
            read(b"POST / HTTP/1.1\r\nContent-Length: ten\r\n\r\n")
                .unwrap_err()
                .0,
            400
        );
        assert_eq!(
            read(b"POST / HTTP/1.1\r\nContent-Length: 9\r\n\r\nshort")
                .unwrap_err()
                .0,
            400
        );
        let big = format!("POST / HTTP/1.1\r\nContent-Length: {MAX_REQUEST}\r\n\r\n");
        assert_eq!(read(big.as_bytes()).unwrap_err().0, 413);
        let mut endless = b"GET / HTTP/1.1\r\nX: ".to_vec();
        endless.resize(MAX_REQUEST + 8192, b'a');
        assert_eq!(read(&endless).unwrap_err().0, 413);
        // Calibration: the same shapes, well formed, read.
        assert!(read(b"GET / HTTP/1.1\r\nHost: x\r\n\r\n").is_ok());
        assert!(read(b"POST / HTTP/1.1\r\nContent-Length: 5\r\n\r\nshort").is_ok());
    }

    #[test]
    fn a_request_past_its_deadline_is_refused() {
        let r = read_request(
            &mut io::Cursor::new(b"GET / HTTP/1.1\r\n".to_vec()),
            MAX_REQUEST,
            Instant::now(),
        );
        assert_eq!(r.unwrap_err().0, 408);
    }

    #[test]
    fn escapes_round_trip_and_broken_ones_are_refused() {
        for s in ["", "a b", "repo/*", "ü&=?%"] {
            assert_eq!(unescape(&escape(s)).as_deref(), Some(s));
        }
        assert_eq!(unescape("%4"), None);
        assert_eq!(unescape("%ff"), None);
        assert_eq!(unescape("a+b").as_deref(), Some("a b"));
    }

    #[test]
    fn urls_are_host_and_port() {
        assert_eq!(parse_url("http://dev:8025"), Ok(("dev".into(), 8025)));
        assert_eq!(parse_url("http://dev:8025/"), Ok(("dev".into(), 8025)));
        assert_eq!(parse_url("http://dev"), Ok(("dev".into(), 80)));
        assert!(parse_url("https://dev:8025").is_err());
        assert!(parse_url("http://dev:x").is_err());
        assert!(parse_url("http://dev:8025/path").is_err());
        assert!(parse_url("http://").is_err());
    }

    #[test]
    fn a_server_and_the_client_talk() {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let handler: Handler = Arc::new(|req: Request| {
            Response::json(
                201,
                format!(
                    "{} {} {}",
                    req.method,
                    req.param("q").unwrap_or("-"),
                    String::from_utf8_lossy(&req.body)
                ),
            )
        });
        std::thread::spawn(move || serve(l, 2, handler));
        let url = format!("http://{addr}");
        let (status, body) = call(
            &url,
            "POST",
            "/x?q=a%20b",
            &[("X-A", "1")],
            b"body",
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(status, 201);
        assert_eq!(body, b"POST a b body");
        // A request the server cannot read is answered, not dropped.
        let mut s = TcpStream::connect(addr).unwrap();
        s.write_all(b"BROKEN\r\n\r\n").unwrap();
        let mut got = Vec::new();
        s.read_to_end(&mut got).unwrap();
        assert_eq!(parse_response(&got).unwrap().0, 400);
    }

    #[test]
    fn an_answer_is_read_to_its_content_length() {
        let r = parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nokEXTRA").unwrap();
        assert_eq!(r, (200, b"ok".to_vec()));
        assert!(parse_response(b"HTTP/1.1 200 OK\r\nContent-Length: 9\r\n\r\nok").is_err());
        assert!(parse_response(b"garbage").is_err());
    }
}
