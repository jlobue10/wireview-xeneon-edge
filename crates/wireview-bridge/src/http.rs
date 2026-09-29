//! Just enough HTTP/1.x for the bridge: parse one request head, write one
//! reply, close. There are no request bodies and no keep-alive, so a
//! connection costs one bounded read and one write.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub const MAX_HEAD_BYTES: usize = 64 * 1024;
pub const MAX_HEADERS: usize = 100;

/// A header that may appear once was sent more than once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Repeated;

#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    /// Path without the query string, not percent-decoded.
    pub path: String,
    pub query: String,
    headers: Vec<(String, String)>,
}

impl Request {
    /// The value of header `name`; `Err` when it was sent more than once.
    pub fn header(&self, name: &str) -> Result<Option<&str>, Repeated> {
        let mut found = self.headers.iter().filter(|(k, _)| k.eq_ignore_ascii_case(name));
        let first = found.next().map(|(_, v)| v.as_str());
        if found.next().is_some() { Err(Repeated) } else { Ok(first) }
    }

    /// First value of query parameter `key`, as sent.
    pub fn query_param(&self, key: &str) -> Option<&str> {
        self.query
            .split('&')
            .filter_map(|kv| kv.split_once('='))
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum ReadError {
    /// The peer sent nothing usable, went quiet, or hung up: just close.
    Gone,
    Malformed(&'static str),
    TooLarge,
}

fn head_end(buf: &[u8], from: usize) -> Option<usize> {
    let start = from.saturating_sub(3);
    let b = &buf[start..];
    (0..b.len()).find_map(|i| {
        if b[i..].starts_with(b"\r\n\r\n") {
            Some(start + i + 4)
        } else if b[i..].starts_with(b"\n\n") {
            Some(start + i + 2)
        } else {
            None
        }
    })
}

/// Read one request head. Each socket read waits at most `timeout`, and the
/// whole head must arrive within `timeout` too, so a peer that trickles bytes
/// holds a worker no longer than one that sends nothing.
pub fn read_request(stream: &mut TcpStream, timeout: Duration) -> Result<Request, ReadError> {
    let deadline = Instant::now() + timeout;
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 2048];
    let end = loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            return Err(ReadError::Gone);
        }
        let n = match stream.read(&mut chunk) {
            Ok(0) => return Err(ReadError::Gone),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return Err(ReadError::Gone),
        };
        let from = buf.len();
        buf.extend_from_slice(&chunk[..n]);
        if let Some(end) = head_end(&buf, from) {
            break end;
        }
        if buf.len() > MAX_HEAD_BYTES {
            return Err(ReadError::TooLarge);
        }
    };
    if end > MAX_HEAD_BYTES {
        return Err(ReadError::TooLarge);
    }
    parse_head(&buf[..end])
}

pub fn parse_head(head: &[u8]) -> Result<Request, ReadError> {
    let head = std::str::from_utf8(head).map_err(|_| ReadError::Malformed("request is not text"))?;
    let mut lines = head.lines();
    let request_line = lines.next().unwrap_or_default();
    let mut parts = request_line.split(' ');
    let (Some(method), Some(target), Some(version), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
        return Err(ReadError::Malformed("bad request line"));
    };
    if !version.starts_with("HTTP/1.") || method.is_empty() || !method.bytes().all(|c| c.is_ascii_uppercase()) {
        return Err(ReadError::Malformed("bad request line"));
    }
    if target.bytes().any(|c| c.is_ascii_control()) {
        return Err(ReadError::Malformed("bad request target"));
    }
    // Absolute form (`GET http://host/path`): keep the path.
    let target = match target.split_once("://") {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") => {
            rest.find('/').map_or("/", |i| &rest[i..])
        }
        _ => target,
    };
    let target = target.split('#').next().unwrap_or_default();
    let (path, query) = target.split_once('?').unwrap_or((target, ""));

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        if headers.len() >= MAX_HEADERS {
            return Err(ReadError::TooLarge);
        }
        // Folded or nameless header lines are obsolete and only ever useful for smuggling.
        let Some((k, v)) = line.split_once(':') else {
            return Err(ReadError::Malformed("bad header line"));
        };
        if k.is_empty() || k.bytes().any(|c| c.is_ascii_whitespace() || c.is_ascii_control()) {
            return Err(ReadError::Malformed("bad header name"));
        }
        headers.push((k.to_string(), v.trim().to_string()));
    }
    Ok(Request {
        method: method.to_string(),
        path: path.to_string(),
        query: query.to_string(),
        headers,
    })
}

#[derive(Debug)]
pub struct Response {
    pub status: u16,
    pub headers: Vec<(&'static str, String)>,
    pub body: Vec<u8>,
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        413 => "Content Too Large",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        _ => "Error",
    }
}

impl Response {
    pub fn new(status: u16) -> Self {
        Response {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// A plain-text error saying why.
    pub fn error(status: u16, why: &str) -> Self {
        let mut r = Response::new(status);
        r.body = format!("{status} {}: {why}\n", reason(status)).into_bytes();
        r.header("Content-Type", "text/plain; charset=utf-8");
        r
    }

    pub fn header(&mut self, name: &'static str, value: impl Into<String>) -> &mut Self {
        self.headers.push((name, value.into()));
        self
    }

    pub fn write_to(&self, stream: &mut TcpStream, server: &str) -> io::Result<()> {
        let mut out = format!(
            "HTTP/1.1 {} {}\r\nServer: {server}\r\nDate: {}\r\nConnection: close\r\n",
            self.status,
            reason(self.status),
            http_date(SystemTime::now())
        );
        for (k, v) in &self.headers {
            // Values are built here from checked input; never let one split the head.
            if !v.bytes().any(|c| c == b'\r' || c == b'\n') {
                out += &format!("{k}: {v}\r\n");
            }
        }
        if self.status != 204 {
            out += &format!("Content-Length: {}\r\n", self.body.len());
        }
        out += "\r\n";
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(&self.body);
        stream.write_all(&bytes)?;
        stream.flush()?;
        let _ = stream.shutdown(Shutdown::Write);
        Ok(())
    }
}

/// RFC 9110 date, e.g. `Tue, 29 Sep 2026 23:10:00 GMT`.
pub fn http_date(t: SystemTime) -> String {
    const DAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let secs = t.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (days, rem) = (secs / 86400, secs % 86400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} GMT",
        DAYS[(days % 7) as usize],
        day,
        MONTHS[(month - 1) as usize],
        year,
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_request() {
        let r = parse_head(b"GET /api/wireview?x=1&nonce=abcd HTTP/1.1\r\nHost: localhost:8765\r\norigin:  http://a \r\n\r\n").unwrap();
        assert_eq!(
            (r.method.as_str(), r.path.as_str(), r.query.as_str()),
            ("GET", "/api/wireview", "x=1&nonce=abcd")
        );
        assert_eq!(r.header("host"), Ok(Some("localhost:8765")));
        assert_eq!(r.header("Origin"), Ok(Some("http://a")));
        assert_eq!(r.header("X-None"), Ok(None));
        assert_eq!(r.query_param("nonce"), Some("abcd"));
        assert_eq!(r.query_param("missing"), None);
    }

    #[test]
    fn accepts_bare_newlines_and_absolute_targets() {
        let r = parse_head(b"GET http://localhost:8765/per-wire/?a=b#f HTTP/1.0\nHost: x\n\n").unwrap();
        assert_eq!((r.path.as_str(), r.query.as_str()), ("/per-wire/", "a=b"));
        assert_eq!(parse_head(b"GET http://localhost:8765 HTTP/1.0\n\n").unwrap().path, "/");
    }

    #[test]
    fn repeated_headers_are_reported() {
        let r = parse_head(b"GET / HTTP/1.1\r\nHost: a\r\nHost: b\r\n\r\n").unwrap();
        assert_eq!(r.header("Host"), Err(Repeated));
    }

    #[test]
    fn rejects_malformed_heads() {
        for bad in [
            &b"GET /\r\n\r\n"[..],
            b"GET / HTTP/2\r\n\r\n",
            b"get / HTTP/1.1\r\n\r\n",
            b"GET / HTTP/1.1 extra\r\n\r\n",
            b"GET  / HTTP/1.1\r\n\r\n",
            b"GET /a\tb HTTP/1.1\r\n\r\n",
            b"GET / HTTP/1.1\r\nno colon here\r\n\r\n",
            b"GET / HTTP/1.1\r\nHost: a\r\n folded\r\n\r\n",
            b"GET / HTTP/1.1\r\nBad Name: a\r\n\r\n",
            b"GET / HTTP/1.1\r\n\xff\xfe: a\r\n\r\n",
        ] {
            assert!(
                matches!(parse_head(bad), Err(ReadError::Malformed(_))),
                "{:?}",
                String::from_utf8_lossy(bad)
            );
        }
        let mut many = b"GET / HTTP/1.1\r\n".to_vec();
        for i in 0..=MAX_HEADERS {
            many.extend_from_slice(format!("X-{i}: v\r\n").as_bytes());
        }
        many.extend_from_slice(b"\r\n");
        assert_eq!(parse_head(&many), Err(ReadError::TooLarge));
    }

    #[test]
    fn finds_the_end_of_a_head_split_across_reads() {
        assert_eq!(head_end(b"GET / HTTP/1.1\r\n\r\n", 0), Some(18));
        assert_eq!(head_end(b"GET / HTTP/1.1\r\n\r\n", 17), Some(18));
        assert_eq!(head_end(b"GET / HTTP/1.1\n\nrest", 0), Some(16));
        assert_eq!(head_end(b"GET / HTTP/1.1\r\n", 0), None);
    }

    #[test]
    fn dates() {
        let at = |s: u64| http_date(UNIX_EPOCH + Duration::from_secs(s));
        assert_eq!(at(0), "Thu, 01 Jan 1970 00:00:00 GMT");
        assert_eq!(at(951_782_400), "Tue, 29 Feb 2000 00:00:00 GMT");
        assert_eq!(at(1_790_723_400), "Tue, 29 Sep 2026 23:10:00 GMT");
    }
}
