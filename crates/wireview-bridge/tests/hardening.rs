//! Regression checks for the bridge's access model.
//!
//! The cases mirror the findings of the 2026-09-26 audits (WV-02..WV-06 and
//! the freshness notes) that the 1.x Python test suite covered for the Python
//! bridge; the parser and shaping cases (A*, B*) live next to the code in
//! wireview-core. No hardware is touched: HWiNFO is replaced by a stub.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use wireview_bridge::{Bridge, Config, MAX_WORKERS, Statics, start};
use wireview_core::auth::{AUTH_HEADER, SecretStore, bridge_sign, new_nonce};
use wireview_core::csvlog::{self, CsvLog};
use wireview_core::{Device, Pin, Reader, Readings, Source, unix_time};

const SECRET: &[u8; 64] = &[b's'; 64];
const BIN: &str = env!("CARGO_BIN_EXE_wireview-bridge");

fn stub(age: f64) -> Reader {
    Reader::new(Source::Hwinfo, None, None).with_hwinfo_backend(move || {
        let mut r = Readings::blank("hwinfo");
        r.ok = true;
        r.hwinfo_running = true;
        r.device_found = true;
        r.poll_time = Some(unix_time() - age);
        r.age_s = Some(age);
        r.total_current = Some(12.5);
        r.total_power = Some(150.0);
        r.avg_voltage = Some(12.0);
        r.cable_w = Some(600);
        r.pins = (1..=6)
            .map(|n| Pin {
                n,
                voltage: Some(12.0),
                current: Some(2.0),
                power: Some(24.0),
            })
            .collect();
        r.device = Some(Device {
            port: Some("COM5".into()),
            fw: Some(5),
            uid: Some("SECRETUID".into()),
            build: Some("x".into()),
        });
        r
    })
}

fn bridge_with(edit: impl FnOnce(&mut Config)) -> Bridge {
    let mut c = Config::new(stub(0.0));
    c.port = 0;
    c.secret = Some(SECRET.to_vec());
    edit(&mut c);
    start(c).expect("bridge starts")
}

fn bridge() -> Bridge {
    bridge_with(|_| {})
}

struct Reply {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.head
            .lines()
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.trim())
    }
}

/// Send raw bytes, read the reply until the server closes.
fn raw(port: u16, request: &[u8]) -> Option<Reply> {
    let mut s = TcpStream::connect(("127.0.0.1", port)).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    s.write_all(request).ok()?;
    let mut buf = Vec::new();
    s.read_to_end(&mut buf).ok()?;
    let split = buf.windows(4).position(|w| w == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&buf[..split]).into_owned();
    let status = head.split(' ').nth(1)?.parse().ok()?;
    Some(Reply {
        status,
        head,
        body: buf[split + 4..].to_vec(),
    })
}

fn request(port: u16, method: &str, path: &str, headers: &[(&str, &str)]) -> Reply {
    let mut req = format!("{method} {path} HTTP/1.1\r\n");
    if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
        req += &format!("Host: 127.0.0.1:{port}\r\n");
    }
    for (k, v) in headers {
        req += &format!("{k}: {v}\r\n");
    }
    req += "\r\n";
    raw(port, req.as_bytes()).expect("a reply")
}

fn get(port: u16, path: &str) -> Reply {
    request(port, "GET", path, &[])
}

fn temp(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!("wireview-bridge-{tag}-{}", new_nonce().unwrap()))
}

#[test]
fn c2_plain_request_is_unsigned_and_hides_the_uid() {
    let b = bridge();
    let r = get(b.port(), "/api/wireview");
    assert_eq!(r.status, 200);
    assert_eq!(r.header(AUTH_HEADER), None);
    assert_eq!(r.header("Content-Type"), Some("application/json"));
    assert_eq!(r.header("Cache-Control"), Some("no-store"));
    let text = String::from_utf8(r.body).unwrap();
    assert!(!text.contains("SECRETUID") && !text.contains("uid"), "{text}");
    let v: serde_json::Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["source"], "hwinfo");
    assert_eq!(v["device"]["port"], "COM5");
    assert_eq!(v["pins"].as_array().map(Vec::len), Some(6));
    assert!(v["served_at"].as_f64().is_some_and(|t| (unix_time() - t).abs() < 5.0));
    assert_eq!(get(b.port(), "/api/wireview/").status, 200);
    b.shutdown();
}

#[test]
fn c3_c4_only_a_valid_nonce_is_signed() {
    let b = bridge();
    let nonce = "ab".repeat(16);
    let r = get(b.port(), &format!("/api/wireview?nonce={nonce}"));
    assert_eq!(r.status, 200);
    assert_eq!(r.header(AUTH_HEADER), Some(bridge_sign(SECRET, &nonce, &r.body).as_str()));
    let r = get(b.port(), &format!("/api/wireview?x=1&nonce={nonce}&nonce=ZZ"));
    assert_eq!(r.header(AUTH_HEADER), Some(bridge_sign(SECRET, &nonce, &r.body).as_str()));
    for bad in ["ZZ", "abcd", &"AB".repeat(16), &"ab".repeat(40), "", "ab%0d%0aX-Evil:1"] {
        let r = get(b.port(), &format!("/api/wireview?nonce={bad}"));
        assert_eq!((r.status, r.header(AUTH_HEADER)), (200, None), "nonce {bad:?}");
        assert!(!r.head.to_lowercase().contains("x-evil"));
    }
    b.shutdown();

    let unsigned = bridge_with(|c| c.secret = None);
    let r = get(unsigned.port(), &format!("/api/wireview?nonce={nonce}"));
    assert_eq!((r.status, r.header(AUTH_HEADER)), (200, None));
    unsigned.shutdown();
}

#[test]
fn health() {
    let b = bridge();
    let r = get(b.port(), "/api/health");
    assert_eq!((r.status, r.body.as_slice()), (200, &br#"{"ok":true}"#[..]));
    b.shutdown();
}

#[test]
fn c5_c6_static_pages_stay_inside_the_root() {
    let b = bridge();
    for p in [
        "/",
        "/per-wire/",
        "/total-current/",
        "/total-power/",
        "/common/wireview.js",
        "/per-wire/?wire_limit=9",
    ] {
        let r = get(b.port(), p);
        assert_eq!(r.status, 200, "{p}");
        assert!(!r.body.is_empty());
    }
    assert!(
        get(b.port(), "/per-wire/")
            .header("Content-Type")
            .is_some_and(|t| t.starts_with("text/html"))
    );
    for p in ["/../Cargo.toml", "/per-wire/../../Cargo.toml", "/nope", "/..%2f..%2fCargo.toml"] {
        assert_eq!(get(b.port(), p).status, 404, "{p}");
    }
    b.shutdown();

    let base = temp("static");
    let root = base.join("docs");
    std::fs::create_dir_all(root.join("nested")).unwrap();
    std::fs::create_dir_all(root.join("per-wire")).unwrap();
    std::fs::write(root.join("per-wire").join("index.html"), "<p>per-wire</p>").unwrap();
    std::fs::write(base.join("OUTSIDE_MARKER.txt"), "outside").unwrap();
    let b = bridge_with(|c| c.statics = Statics::Dir(root.clone()));
    assert_eq!(get(b.port(), "/per-wire/").body, b"<p>per-wire</p>");
    let r = get(b.port(), "/../OUTSIDE_MARKER.txt");
    assert!(r.status == 403 && !r.body.starts_with(b"outside"));
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(base.join("OUTSIDE_MARKER.txt"), root.join("nested").join("index.html")).unwrap();
        let r = get(b.port(), "/nested/");
        assert!(
            r.status == 403 && !r.body.starts_with(b"outside"),
            "index.html symlink escape (WV-06): {}",
            r.status
        );
    }
    b.shutdown();
    std::fs::remove_dir_all(base).unwrap();

    let none = bridge_with(|c| c.statics = Statics::None);
    assert_eq!(get(none.port(), "/per-wire/").status, 404);
    assert_eq!(get(none.port(), "/api/health").status, 200);
    none.shutdown();
}

#[test]
fn c7_cors_allowlist() {
    let b = bridge_with(|c| c.allow_origins = vec!["https://Example.GitHub.io/".into()]);
    let port = b.port();
    for path in ["/api/wireview", "/api/health", "/per-wire/"] {
        let r = request(port, "GET", path, &[("Origin", "https://evil.example.com")]);
        assert_eq!(r.status, 403, "{path}");
        assert_eq!(r.header("Access-Control-Allow-Origin"), None);
        assert!(!String::from_utf8_lossy(&r.body).contains("total_current"));
    }
    assert_eq!(
        request(port, "OPTIONS", "/api/wireview", &[("Origin", "https://evil.example.com")]).status,
        403
    );
    assert_eq!(request(port, "GET", "/api/wireview", &[("Origin", "null")]).status, 403);

    let local = format!("http://localhost:{port}");
    for origin in [
        "https://jlobue10.github.io",
        "https://example.github.io",
        local.as_str(),
        "HTTPS://JLOBUE10.github.io/",
    ] {
        let r = request(port, "GET", "/api/wireview", &[("Origin", origin)]);
        assert_eq!(r.status, 200, "{origin}");
        let echoed = origin.trim_end_matches('/').to_lowercase();
        assert_eq!(r.header("Access-Control-Allow-Origin"), Some(echoed.as_str()));
        assert_eq!(r.header("Vary"), Some("Origin"));
    }
    let r = request(port, "OPTIONS", "/api/wireview", &[("Origin", "https://jlobue10.github.io")]);
    assert_eq!((r.status, r.body.len()), (204, 0));
    assert_eq!(r.header("Access-Control-Allow-Methods"), Some("GET, OPTIONS"));
    assert_eq!(r.header("Access-Control-Allow-Private-Network"), Some("true"));
    // No Origin at all: same-origin page, curl, the Nexus daemon.
    assert_eq!(get(port, "/api/wireview").header("Access-Control-Allow-Origin"), None);
    let twice = raw(
        port,
        format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: {local}\r\nOrigin: https://evil.example.com\r\n\r\n").as_bytes(),
    );
    assert_eq!(twice.unwrap().status, 400);
    b.shutdown();
}

#[test]
fn host_header_must_name_loopback() {
    let b = bridge();
    let port = b.port();
    for good in [
        format!("127.0.0.1:{port}"),
        format!("localhost:{port}"),
        format!("LOCALHOST:{port}"),
        format!("[::1]:{port}"),
    ] {
        assert_eq!(request(port, "GET", "/api/health", &[("Host", &good)]).status, 200, "{good}");
    }
    for bad in [
        "evil.example.com".to_string(),
        format!("evil.example.com:{port}"),
        "127.0.0.1".into(),
        format!("127.0.0.1:{}", port + 1),
        String::new(),
    ] {
        assert_eq!(request(port, "GET", "/api/wireview", &[("Host", &bad)]).status, 403, "{bad:?}");
    }
    assert_eq!(raw(port, b"GET /api/wireview HTTP/1.0\r\n\r\n").unwrap().status, 403);
    let twice = raw(
        port,
        format!("GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nHost: evil.example.com\r\n\r\n").as_bytes(),
    );
    assert_eq!(twice.unwrap().status, 400);
    b.shutdown();
}

#[test]
fn only_get_and_options() {
    let b = bridge();
    for method in ["POST", "PUT", "DELETE", "HEAD", "PATCH"] {
        assert_eq!(request(b.port(), method, "/api/wireview", &[]).status, 501, "{method}");
    }
    assert_eq!(raw(b.port(), b"nonsense\r\n\r\n").unwrap().status, 400);
    let mut huge = b"GET / HTTP/1.1\r\nX-Pad: ".to_vec();
    huge.extend(std::iter::repeat_n(b'a', 80 * 1024));
    huge.extend_from_slice(b"\r\n\r\n");
    // The server may close while the rest is still being sent; when a reply arrives it is 431.
    if let Some(r) = raw(b.port(), &huge) {
        assert_eq!(r.status, 431);
    }
    assert_eq!(get(b.port(), "/api/health").status, 200);
    b.shutdown();
}

#[test]
fn c8_client_accepts_the_authenticated_bridge() {
    let b = bridge();
    let file = temp("secret");
    std::fs::write(&file, SECRET).unwrap();
    for host in ["localhost", "127.0.0.1", "[::1]"] {
        let url = format!("http://{host}:{}/api/wireview", b.port());
        let client = Reader::new(Source::Bridge, None, Some(url)).with_secrets(SecretStore::at(&file));
        let out = client.read();
        assert!(out.ok, "{host}: {out:?} / {:?}", client.bridge_error());
        assert_eq!(out.source, "bridge");
        assert_eq!((out.total_current, out.cable_w, out.pins.len()), (Some(12.5), Some(600), 6));
        assert_eq!(out.device.and_then(|d| d.uid), None);
    }
    // The same bridge, another secret: not trusted.
    std::fs::write(&file, [b'x'; 64]).unwrap();
    let url = format!("http://127.0.0.1:{}/api/wireview", b.port());
    let client = Reader::new(Source::Bridge, None, Some(url)).with_secrets(SecretStore::at(&file));
    assert!(!client.read().ok);
    assert!(client.bridge_error().is_some_and(|e| e.contains("authentication")));
    std::fs::remove_file(file).unwrap();
    b.shutdown();
}

fn wait_for(what: &str, limit: Duration, mut done: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !done() {
        assert!(t0.elapsed() < limit, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn c11_worker_cap_holds_and_idle_peers_are_dropped() {
    let b = bridge_with(|c| c.request_timeout = Duration::from_millis(1500));
    let port = b.port();
    // 40 connections that never finish their request.
    let idle: Vec<TcpStream> = (0..40)
        .map(|_| {
            let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
            s.write_all(format!("GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n").as_bytes())
                .unwrap();
            s
        })
        .collect();
    wait_for("the workers to fill", Duration::from_secs(2), || b.workers() == MAX_WORKERS);
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(b.workers(), MAX_WORKERS, "40 half-open connections (WV-05)");
    // Over the cap, a new connection is closed without an answer.
    assert!(
        raw(
            port,
            format!("GET /api/health HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n").as_bytes()
        )
        .is_none()
    );
    // The idle peers are dropped after the timeout, without being closed by the client.
    wait_for("idle workers to be dropped", Duration::from_secs(4), || b.workers() == 0);
    assert_eq!(get(port, "/api/health").status, 200);
    drop(idle);
    b.shutdown();
}

#[test]
fn a_trickling_request_cannot_hold_a_worker() {
    let b = bridge_with(|c| c.request_timeout = Duration::from_millis(800));
    let mut s = TcpStream::connect(("127.0.0.1", b.port())).unwrap();
    let t0 = Instant::now();
    let mut closed = false;
    // One byte every 100 ms keeps each read alive; the head deadline must still end it.
    for byte in std::iter::repeat_n(b'G', 40) {
        if s.write_all(&[byte]).is_err() {
            closed = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
        if b.workers() == 0 {
            closed = true;
            break;
        }
    }
    assert!(
        closed && t0.elapsed() < Duration::from_secs(3),
        "worker held for {:?}",
        t0.elapsed()
    );
    b.shutdown();
}

#[test]
fn a_slow_response_reader_cannot_restart_the_worker_deadline() {
    use socket2::{Domain, Protocol, Socket, Type};

    let root = temp("slow-reader");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("large.bin"), vec![0u8; 8 * 1024 * 1024]).unwrap();
    let b = bridge_with(|c| {
        c.request_timeout = Duration::from_millis(200);
        c.statics = Statics::Dir(root.clone());
    });
    let sock = Socket::new(Domain::IPV4, Type::STREAM, Some(Protocol::TCP)).unwrap();
    sock.set_recv_buffer_size(8192).unwrap();
    sock.connect(&SocketAddr::from(([127, 0, 0, 1], b.port())).into()).unwrap();
    let mut stream: TcpStream = sock.into();
    stream.set_read_timeout(Some(Duration::from_millis(100))).unwrap();
    stream
        .write_all(format!("GET /large.bin HTTP/1.1\r\nHost: 127.0.0.1:{}\r\n\r\n", b.port()).as_bytes())
        .unwrap();
    wait_for("slow reader to occupy a worker", Duration::from_secs(1), || b.workers() == 1);
    let started = Instant::now();
    let mut bytes = [0u8; 8192];
    while b.workers() != 0 && started.elapsed() < Duration::from_secs(1) {
        let _ = stream.read(&mut bytes);
        std::thread::sleep(Duration::from_millis(40));
    }
    assert_eq!(b.workers(), 0, "slow reader held a worker past the absolute deadline");
    drop(stream);
    b.shutdown();
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn c12_listens_on_both_loopback_families() {
    let b = bridge();
    let addrs = b.addrs();
    assert!(
        addrs.iter().any(SocketAddr::is_ipv4) && addrs.iter().any(SocketAddr::is_ipv6),
        "{addrs:?}"
    );
    assert!(addrs.iter().all(|a| a.ip().is_loopback() && a.port() == b.port()));
    let mut s = TcpStream::connect(("::1", b.port())).unwrap();
    s.write_all(format!("GET /api/health HTTP/1.0\r\nHost: [::1]:{}\r\n\r\n", b.port()).as_bytes())
        .unwrap();
    let mut reply = String::new();
    s.read_to_string(&mut reply).unwrap();
    assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
    b.shutdown();
}

#[test]
fn c13_stale_source_is_reported_not_ok() {
    let b = bridge_with(|c| c.reader = stub(30.0));
    let v: serde_json::Value = serde_json::from_slice(&get(b.port(), "/api/wireview").body).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["status"], "Stale readings");
    b.shutdown();
}

#[test]
fn c14_a_taken_loopback_address_refuses_to_start() {
    // Occupy [::1]:port only; 127.0.0.1:port is free.
    let occupied = TcpListener::bind("[::1]:0").unwrap();
    let port = occupied.local_addr().unwrap().port();
    let mut c = Config::new(stub(0.0));
    c.port = port;
    let err = start(c).err().expect("must not start half-bound (WV-03b)").to_string();
    assert!(err.contains(&format!("cannot bind ::1:{port}")), "{err}");
    // Nothing was left listening on the IPv4 side.
    assert!(TcpStream::connect_timeout(&format!("127.0.0.1:{port}").parse().unwrap(), Duration::from_millis(300)).is_err());

    let p = Command::new(BIN)
        .args(["--port", &port.to_string(), "--source", "hwinfo", "--no-static"])
        .env("WIREVIEW_BRIDGE_SECRET", temp("collide"))
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&p.stdout).into_owned() + &String::from_utf8_lossy(&p.stderr);
    assert!(!p.status.success() && out.contains("cannot bind ::1"), "{out}");
}

#[test]
fn non_loopback_bind_names_are_refused_when_meaningless() {
    let mut c = Config::new(stub(0.0));
    c.port = 0;
    c.bind = "no such host.invalid".into();
    assert!(start(c).is_err());
}

#[test]
fn binary_arguments() {
    let run = |args: &[&str]| {
        let p = Command::new(BIN)
            .args(args)
            .env("WIREVIEW_BRIDGE_SECRET", temp("args"))
            .output()
            .unwrap();
        (
            p.status.code(),
            String::from_utf8_lossy(&p.stdout).into_owned(),
            String::from_utf8_lossy(&p.stderr).into_owned(),
        )
    };
    let (code, out, _) = run(&["--version"]);
    assert_eq!(
        (code, out.trim()),
        (Some(0), concat!("wireview-bridge ", env!("CARGO_PKG_VERSION")))
    );
    for bad in [
        &["--source", "bridge"][..],
        &["--source", "usb"],
        &["--port", "0"],
        &["--port", "70000"],
        &["--no-static", "--static-dir", "."],
        &["--static-dir", "/no/such/dir"],
        &["--csv-interval", "5"],
        &["--csv-log", "x", "--csv-interval", "0"],
        &["--csv-log", "x", "--csv-interval", "forever"],
    ] {
        let (code, _, err) = run(bad);
        assert_eq!(code, Some(2), "{bad:?}: {err}");
    }
    // A log directory that cannot be created is refused before anything listens.
    let unwritable = temp("csv-unwritable");
    std::fs::write(&unwritable, b"a file, not a directory").unwrap();
    let (code, _, err) = run(&["--csv-log", unwritable.join("logs").to_str().unwrap(), "--port", "1"]);
    assert_eq!(code, Some(2), "{err}");
    assert!(err.contains("cannot create the log file"), "{err}");
    std::fs::remove_file(unwritable).unwrap();
}

#[test]
fn csv_log_writes_the_header_and_rows_from_its_own_thread_and_stops_with_the_bridge() {
    let dir = temp("csv");
    let log = CsvLog::create(&dir, csvlog::MIN_INTERVAL).unwrap();
    let path = log.path().to_path_buf();
    let b = bridge_with(|c| c.csv_log = Some(log));
    // The first row is due at once, the second after the interval; no request is needed.
    wait_for("two CSV rows", Duration::from_secs(5), || {
        std::fs::read_to_string(&path).is_ok_and(|t| {
            t.matches("\r\n")
            .count()
                >= 3
        })
    });
    let started = Instant::now();
    b.shutdown();
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "the log thread must notice the shutdown"
    );
    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.split("\r\n").collect();
    assert_eq!(lines[0], csvlog::HEADER);
    let row: Vec<&str> = lines[1].split(',').collect();
    assert_eq!(row.len(), 22, "{}", lines[1]);
    assert_eq!(&row[1..6], ["True", "", "5", "150.000", "12.500"]);
    assert_eq!(&row[10..16], ["12.000"; 6]);
    assert_eq!(&row[16..22], ["2.000"; 6]);
    assert!(
        row[0].len() == 27 && row[0].as_bytes()[10] == b'T' && row[0].as_bytes()[19] == b'.',
        "{}",
        row[0]
    );
    assert!(!text.contains("SECRETUID"));
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn c1_binary_creates_the_secret_and_serves() {
    let secret = temp("c1");
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let mut child = Command::new(BIN)
        .args(["--port", &port.to_string(), "--source", "hwinfo"])
        .env("WIREVIEW_BRIDGE_SECRET", &secret)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for("the bridge to listen", Duration::from_secs(5), || {
        TcpStream::connect(("127.0.0.1", port)).is_ok()
    });
    assert!(std::fs::read(&secret).is_ok_and(|s| s.len() >= 32));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&secret).unwrap().permissions().mode() & 0o777, 0o600);
    }
    let nonce = "0123456789abcdef".repeat(2);
    let r = get(port, &format!("/api/wireview?nonce={nonce}"));
    let key = std::fs::read(&secret).unwrap();
    assert_eq!(r.header(AUTH_HEADER), Some(bridge_sign(key.trim_ascii(), &nonce, &r.body).as_str()));
    assert_eq!(r.header("Server"), Some(concat!("WireViewBridge/", env!("CARGO_PKG_VERSION"))));
    let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
    assert_eq!(v["ok"], false); // no HWiNFO here
    assert_eq!(get(port, "/per-wire/").status, 200);
    child.kill().unwrap();
    let mut out = String::new();
    child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
    child.wait().unwrap();
    assert!(out.contains("127.0.0.1") && out.contains("[::1]"), "{out}");
    assert!(out.contains("readings: none (HWiNFO not running"), "{out}");
    std::fs::remove_file(secret).unwrap();
}
