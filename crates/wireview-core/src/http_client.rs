//! The one HTTP request this crate makes: a GET to the local bridge.
//!
//! Deliberately small instead of a general client. The connection goes
//! straight to the address in the URL (no proxy from the environment is ever
//! consulted), every socket operation has a short timeout, the whole exchange
//! must finish before a deadline, and the reply is capped in size. Whatever
//! answers on the bridge port is just another local process, so a server
//! that trickles bytes or never stops sending cannot stall the caller.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

pub(crate) struct Limits {
    /// Per blocking socket operation.
    pub op_timeout: Duration,
    /// For the whole request, connect to last byte.
    pub deadline: Duration,
    pub max_body: usize,
}

const MAX_HEAD: usize = 16 * 1024;

#[derive(Debug)]
pub(crate) struct Response {
    headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, PartialEq, Eq)]
struct Target<'a> {
    /// `host[:port]` exactly as written, for the Host header.
    authority: &'a str,
    host: &'a str,
    port: u16,
    /// Path and query, never empty.
    request: &'a str,
}

fn parse_url(url: &str) -> Result<Target<'_>, String> {
    let bad = |why: &str| format!("bad bridge URL {url:?}: {why}");
    if url.bytes().any(|c| c.is_ascii_control() || c == b' ') {
        return Err(bad("contains whitespace or control characters"));
    }
    let scheme = url.get(..7).filter(|s| s.eq_ignore_ascii_case("http://"));
    let rest = match scheme {
        Some(_) => &url[7..],
        None => return Err(bad("only http:// is supported")),
    };
    let rest = rest.split('#').next().unwrap_or_default();
    let cut = rest.find(['/', '?']).unwrap_or(rest.len());
    let (authority, request) = rest.split_at(cut);
    if authority.is_empty() || authority.contains('@') {
        return Err(bad("missing host"));
    }
    let (host, port) = match authority.strip_prefix('[') {
        Some(v6) => {
            let (host, after) = v6.split_once(']').ok_or_else(|| bad("unterminated IPv6 address"))?;
            (host, after.strip_prefix(':'))
        }
        None => match authority.rsplit_once(':') {
            Some((host, port)) => (host, Some(port)),
            None => (authority, None),
        },
    };
    let port = match port {
        Some(p) => p.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(|| bad("bad port"))?,
        None => 80,
    };
    if host.is_empty() {
        return Err(bad("missing host"));
    }
    Ok(Target {
        authority,
        host,
        port,
        request,
    })
}

/// Append a query parameter to `url`.
pub(crate) fn with_query(url: &str, key: &str, value: &str) -> String {
    let (base, fragment) = url.split_once('#').map_or((url, None), |(b, f)| (b, Some(f)));
    let sep = if base.contains('?') { '&' } else { '?' };
    match fragment {
        Some(f) => format!("{base}{sep}{key}={value}#{f}"),
        None => format!("{base}{sep}{key}={value}"),
    }
}

fn left(deadline: Instant, op: Duration) -> Result<Duration, String> {
    let left = deadline.saturating_duration_since(Instant::now());
    if left.is_zero() {
        return Err("bridge reply exceeded the deadline".into());
    }
    Ok(left.min(op))
}

fn connect(t: &Target<'_>, deadline: Instant, op: Duration) -> Result<TcpStream, String> {
    let addrs = (t.host, t.port)
        .to_socket_addrs()
        .map_err(|e| format!("cannot resolve {}: {e}", t.host))?;
    let mut last = format!("{} has no address", t.host);
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, left(deadline, op)?) {
            Ok(s) => return Ok(s),
            Err(e) => last = format!("cannot connect to {addr}: {e}"),
        }
    }
    Err(last)
}

pub(crate) fn get(url: &str, limits: &Limits) -> Result<Response, String> {
    let deadline = Instant::now() + limits.deadline;
    let target = parse_url(url)?;
    let mut stream = connect(&target, deadline, limits.op_timeout)?;
    let _ = stream.set_nodelay(true);
    let io = |e: std::io::Error| match e.kind() {
        std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => "bridge reply timed out".to_string(),
        _ => e.to_string(),
    };

    // HTTP/1.0 keeps the reply unchunked and closed when done.
    let path = if target.request.starts_with('/') {
        target.request.to_string()
    } else {
        format!("/{}", target.request)
    };
    let request = format!(
        "GET {path} HTTP/1.0\r\nHost: {}\r\nUser-Agent: wireview/{}\r\nAccept: application/json\r\nConnection: close\r\n\r\n",
        target.authority,
        crate::VERSION
    );
    stream.set_write_timeout(Some(left(deadline, limits.op_timeout)?)).map_err(io)?;
    stream.write_all(request.as_bytes()).map_err(io)?;

    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut chunk = [0u8; 4096];
    let mut head_end: Option<usize> = None;
    let mut want: Option<usize> = None; // Content-Length
    loop {
        if let (Some(h), Some(n)) = (head_end, want) {
            if buf.len() >= h + n {
                break;
            }
        }
        stream.set_read_timeout(Some(left(deadline, limits.op_timeout)?)).map_err(io)?;
        let n = match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(io(e)),
        };
        let scan_from = buf.len().saturating_sub(3);
        buf.extend_from_slice(&chunk[..n]);
        if head_end.is_none() {
            if let Some(i) = buf[scan_from..].windows(4).position(|w| w == b"\r\n\r\n") {
                let end = scan_from + i + 4;
                head_end = Some(end);
                want = content_length(&buf[..end])?;
                if want.is_some_and(|n| n > limits.max_body) {
                    return Err("bridge reply too large".into());
                }
            } else if buf.len() > MAX_HEAD {
                return Err("bridge reply headers too large".into());
            }
        }
        if let Some(h) = head_end {
            if buf.len() - h > limits.max_body {
                return Err("bridge reply too large".into());
            }
        }
    }

    let head_end = head_end.ok_or("bridge closed the connection before replying")?;
    let head = std::str::from_utf8(&buf[..head_end - 4]).map_err(|_| "bridge reply headers are not text")?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let mut parts = status_line.splitn(3, ' ');
    let (version, status) = (parts.next().unwrap_or_default(), parts.next().unwrap_or_default());
    if !version.starts_with("HTTP/1.") {
        return Err(format!("not an HTTP reply: {:?}", status_line.chars().take(40).collect::<String>()));
    }
    if status != "200" {
        return Err(format!("bridge answered HTTP {status}"));
    }
    let headers = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
        .collect();
    let mut body = buf.split_off(head_end);
    if let Some(n) = want {
        if body.len() < n {
            return Err("bridge reply was cut short".into());
        }
        body.truncate(n);
    }
    Ok(Response { headers, body })
}

fn content_length(head: &[u8]) -> Result<Option<usize>, String> {
    let head = std::str::from_utf8(head).map_err(|_| "bridge reply headers are not text")?;
    for line in head.split("\r\n").skip(1) {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                return v
                    .trim()
                    .parse()
                    .map(Some)
                    .map_err(|_| "bad Content-Length in bridge reply".to_string());
            }
        }
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::thread;

    use super::*;

    const LIMITS: Limits = Limits {
        op_timeout: Duration::from_millis(500),
        deadline: Duration::from_secs(1),
        max_body: 64 * 1024,
    };

    #[test]
    fn urls() {
        let t = parse_url("http://localhost:8765/api/wireview").unwrap();
        assert_eq!(
            t,
            Target {
                authority: "localhost:8765",
                host: "localhost",
                port: 8765,
                request: "/api/wireview"
            }
        );
        let t = parse_url("HTTP://[::1]:9000/x?a=1#frag").unwrap();
        assert_eq!(
            t,
            Target {
                authority: "[::1]:9000",
                host: "::1",
                port: 9000,
                request: "/x?a=1"
            }
        );
        let t = parse_url("http://example").unwrap();
        assert_eq!((t.host, t.port, t.request), ("example", 80, ""));
        for bad in [
            "https://localhost/",
            "localhost:8765",
            "http://",
            "http://:80/",
            "http://h:0/",
            "http://h:x/",
            "http://u@h/",
            "http://h/a b",
            "http://h/\r\nX: y",
        ] {
            assert!(parse_url(bad).is_err(), "{bad}");
        }
        assert_eq!(with_query("http://h/p", "nonce", "ab"), "http://h/p?nonce=ab");
        assert_eq!(with_query("http://h/p?x=1", "nonce", "ab"), "http://h/p?x=1&nonce=ab");
    }

    /// Serve one connection with `reply`, then hold it open for `linger`.
    fn serve(reply: Vec<Vec<u8>>, gap: Duration) -> String {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/api/wireview", l.local_addr().unwrap());
        thread::spawn(move || {
            let (mut s, _) = l.accept().unwrap();
            let mut req = [0u8; 2048];
            let _ = s.read(&mut req);
            for part in reply {
                if s.write_all(&part).is_err() {
                    return;
                }
                thread::sleep(gap);
            }
        });
        url
    }

    #[test]
    fn reads_a_reply_and_its_headers() {
        let url = serve(
            vec![b"HTTP/1.0 200 OK\r\nx-wireview-auth: abc\r\nContent-Length: 2\r\n\r\n{}".to_vec()],
            Duration::ZERO,
        );
        let r = get(&url, &LIMITS).unwrap();
        assert_eq!(r.body, b"{}");
        assert_eq!(r.header("X-WireView-Auth"), Some("abc"));
    }

    #[test]
    fn reads_until_close_without_a_length() {
        let url = serve(
            vec![b"HTTP/1.1 200 OK\r\n\r\nhel".to_vec(), b"lo".to_vec()],
            Duration::from_millis(20),
        );
        assert_eq!(get(&url, &LIMITS).unwrap().body, b"hello");
    }

    #[test]
    fn refuses_errors_oversize_and_truncation() {
        let url = serve(
            vec![b"HTTP/1.0 403 Forbidden\r\nContent-Length: 0\r\n\r\n".to_vec()],
            Duration::ZERO,
        );
        assert!(get(&url, &LIMITS).unwrap_err().contains("403"));
        let url = serve(vec![b"HTTP/1.0 200 OK\r\nContent-Length: 999999\r\n\r\n".to_vec()], Duration::ZERO);
        assert!(get(&url, &LIMITS).unwrap_err().contains("too large"));
        let mut big = b"HTTP/1.0 200 OK\r\n\r\n".to_vec();
        big.extend(std::iter::repeat_n(b'x', 70 * 1024));
        let url = serve(vec![big], Duration::ZERO);
        assert!(get(&url, &LIMITS).unwrap_err().contains("too large"));
        let url = serve(vec![b"HTTP/1.0 200 OK\r\nContent-Length: 10\r\n\r\nabc".to_vec()], Duration::ZERO);
        assert!(get(&url, &LIMITS).unwrap_err().contains("cut short"));
        let url = serve(vec![b"SSH-2.0-OpenSSH\r\n\r\n".to_vec()], Duration::ZERO);
        assert!(get(&url, &LIMITS).unwrap_err().contains("not an HTTP reply"));
    }

    #[test]
    fn a_trickling_server_is_cut_off_by_the_deadline() {
        let mut parts = vec![b"HTTP/1.0 200 OK\r\nContent-Length: 40\r\n\r\n".to_vec()];
        parts.extend(std::iter::repeat_n(b" ".to_vec(), 40));
        let url = serve(parts, Duration::from_millis(100));
        let t0 = Instant::now();
        let err = get(&url, &LIMITS).unwrap_err();
        assert!(t0.elapsed() < Duration::from_millis(1500), "{:?}", t0.elapsed());
        assert!(err.contains("deadline") || err.contains("timed out"), "{err}");
    }

    #[test]
    fn nothing_listening_fails_fast() {
        let port = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
        let t0 = Instant::now();
        assert!(get(&format!("http://127.0.0.1:{port}/"), &LIMITS).is_err());
        assert!(t0.elapsed() < Duration::from_millis(1500));
    }
}
