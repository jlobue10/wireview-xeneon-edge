//! Local bridge: serves WireView Pro II readings as JSON on localhost.
//!
//! The Xeneon Edge widgets (hosted on GitHub Pages or served from this
//! bridge) poll `http://localhost:8765/api/wireview` once a second. The
//! bridge reads the WireView directly over USB serial (falling back to HWiNFO
//! shared memory), caches for a short interval, and answers CORS requests
//! only from the widget origins it knows (its own loopback origin and the
//! GitHub Pages copy), so an arbitrary website open in a browser cannot read
//! the readings. Requests whose `Host` header is not a loopback name are
//! refused, which also defeats DNS rebinding. The device's hardware UID is
//! not served.
//!
//! Programs that share the device through this bridge (wireview-nexus) send a
//! `nonce` query parameter; the reply carries `X-WireView-Auth`, an HMAC over
//! nonce and body keyed with a per-user secret file, so a client can tell
//! this bridge from any other process that happens to own the port.

pub mod http;
pub mod statics;

use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use socket2::{Domain, Protocol, Socket, Type};
use wireview_core::auth::{AUTH_HEADER, bridge_sign, valid_nonce};
use wireview_core::{CsvLog, Reader, Readings, unix_time};

use crate::http::{ReadError, Repeated, Request, Response, read_request};
use crate::statics::Found;
pub use crate::statics::Statics;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const DEFAULT_PORT: u16 = 8765;
/// The hosted copy of `docs/`.
pub const PAGES_ORIGIN: &str = "https://jlobue10.github.io";
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "[::1]"];
const CACHE: Duration = Duration::from_millis(250);
/// A peer that has not sent its request, or taken its reply, by then is dropped.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Concurrent connections, over both listeners.
pub const MAX_WORKERS: usize = 32;
const LOG_ENV: &str = "WIREVIEW_BRIDGE_LOG";

pub struct Config {
    /// 0 picks a free port (tests).
    pub port: u16,
    pub bind: String,
    /// Extra web origins allowed to read the API.
    pub allow_origins: Vec<String>,
    pub statics: Statics,
    pub reader: Reader,
    /// Key for `X-WireView-Auth`; without one no reply is signed.
    pub secret: Option<Vec<u8>>,
    pub request_timeout: Duration,
    /// Append the readings to this CSV log at its interval.
    pub csv_log: Option<CsvLog>,
}

impl Config {
    pub fn new(reader: Reader) -> Self {
        Config {
            port: DEFAULT_PORT,
            bind: "127.0.0.1".into(),
            allow_origins: Vec::new(),
            statics: Statics::Embedded,
            reader,
            secret: None,
            request_timeout: REQUEST_TIMEOUT,
            csv_log: None,
        }
    }
}

#[derive(Debug)]
pub enum StartError {
    /// `--bind` names nothing that can be bound.
    BadBind(String),
    /// An address is taken or otherwise refused.
    Bind { addr: SocketAddr, error: io::Error },
}

impl fmt::Display for StartError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StartError::BadBind(why) => f.write_str(why),
            StartError::Bind { addr, error } => write!(
                f,
                "cannot bind {}:{}: {error}. Is another bridge (or another program) already listening there?",
                addr.ip(),
                addr.port()
            ),
        }
    }
}

impl std::error::Error for StartError {}

struct Cached {
    at: Option<Instant>,
    body: Arc<Vec<u8>>,
    last_source: Option<String>,
}

struct State {
    reader: Reader,
    cache: Mutex<Cached>,
    statics: Statics,
    allowed_origins: BTreeSet<String>,
    /// `None` = any (non-loopback bind).
    allowed_hosts: Option<BTreeSet<String>>,
    secret: Option<Vec<u8>>,
    request_timeout: Duration,
    workers: AtomicUsize,
    stopping: AtomicBool,
    log_requests: bool,
}

/// A running bridge.
pub struct Bridge {
    state: Arc<State>,
    addrs: Vec<SocketAddr>,
    threads: Vec<JoinHandle<()>>,
}

impl Bridge {
    /// The addresses being listened on.
    pub fn addrs(&self) -> &[SocketAddr] {
        &self.addrs
    }

    pub fn port(&self) -> u16 {
        self.addrs[0].port()
    }

    /// Connections being served right now.
    pub fn workers(&self) -> usize {
        self.state.workers.load(Ordering::SeqCst)
    }

    pub fn serves_pages(&self) -> bool {
        self.state.statics != Statics::None
    }

    /// Serve until the process ends.
    pub fn wait(self) {
        for t in self.threads {
            let _ = t.join();
        }
    }

    /// Stop accepting and release the ports.
    pub fn shutdown(self) {
        self.state.stopping.store(true, Ordering::SeqCst);
        for addr in &self.addrs {
            // Wake the blocked accept().
            let _ = TcpStream::connect_timeout(addr, Duration::from_millis(500));
        }
        self.wait();
    }
}

fn normalise_origin(o: &str) -> String {
    o.trim().trim_end_matches('/').to_ascii_lowercase()
}

/// This address family does not exist on this machine, so skipping the IPv6
/// listener is correct. Anything else (address in use!) is not.
fn family_unavailable(e: &io::Error) -> bool {
    #[cfg(windows)]
    const CODES: [i32; 3] = [10047, 10049, 10043]; // WSAEAFNOSUPPORT, WSAEADDRNOTAVAIL, WSAEPROTONOSUPPORT
    #[cfg(target_os = "linux")]
    const CODES: [i32; 4] = [97, 99, 96, 93]; // EAFNOSUPPORT, EADDRNOTAVAIL, EPFNOSUPPORT, EPROTONOSUPPORT
    #[cfg(not(any(windows, target_os = "linux")))]
    const CODES: [i32; 4] = [47, 49, 46, 43]; // the same four on the BSDs and macOS
    e.kind() == io::ErrorKind::AddrNotAvailable || e.raw_os_error().is_some_and(|c| CODES.contains(&c))
}

#[cfg(windows)]
fn claim_address(sock: &Socket) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{SO_EXCLUSIVEADDRUSE, SOL_SOCKET, setsockopt};
    // On Windows SO_REUSEADDR lets a second program bind the same port
    // silently, so insist on exclusive use there.
    let on: i32 = 1;
    // SAFETY: the socket is open and `on` outlives the call.
    let rc = unsafe {
        setsockopt(
            sock.as_raw_socket() as _,
            SOL_SOCKET,
            SO_EXCLUSIVEADDRUSE,
            (&raw const on).cast(),
            4,
        )
    };
    if rc == 0 { Ok(()) } else { Err(io::Error::last_os_error()) }
}

#[cfg(not(windows))]
fn claim_address(sock: &Socket) -> io::Result<()> {
    sock.set_reuse_address(true) // the usual fast restart
}

fn listen(addr: SocketAddr) -> io::Result<TcpListener> {
    let sock = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    if addr.is_ipv6() {
        sock.set_only_v6(true)?;
    }
    claim_address(&sock)?;
    sock.bind(&addr.into())?;
    sock.listen(64)?;
    Ok(sock.into())
}

/// Bind and start serving. Refuses to start half-bound: a collision on
/// either loopback family means another process would receive some of the
/// traffic meant for this bridge.
pub fn start(config: Config) -> Result<Bridge, StartError> {
    let v4 = IpAddr::V4(Ipv4Addr::LOCALHOST);
    let v6 = IpAddr::V6(Ipv6Addr::LOCALHOST);
    // Browsers resolve "localhost" to ::1 first on Windows, so listen on both
    // loopback families when binding to the default address.
    let (ips, loopback) = match config.bind.as_str() {
        "127.0.0.1" | "localhost" => (vec![v4, v6], true),
        "::1" | "[::1]" => (vec![v6], true),
        other => {
            let ip = other.trim_matches(['[', ']']).parse::<IpAddr>().ok().or_else(|| {
                (other, config.port)
                    .to_socket_addrs()
                    .ok()
                    .and_then(|mut a| a.next())
                    .map(|a| a.ip())
            });
            match ip {
                Some(ip) => (vec![ip], false),
                None => return Err(StartError::BadBind(format!("cannot bind {other}: not an address of this machine"))),
            }
        }
    };

    let mut listeners: Vec<TcpListener> = Vec::new();
    let mut port = config.port;
    for ip in ips {
        let addr = SocketAddr::new(ip, port);
        match listen(addr) {
            Ok(l) => {
                if port == 0 {
                    port = l.local_addr().map(|a| a.port()).unwrap_or(0);
                }
                listeners.push(l);
            }
            Err(e) if ip == v6 && loopback && !listeners.is_empty() && family_unavailable(&e) => {}
            Err(error) => return Err(StartError::Bind { addr, error }),
        }
    }

    let default_port = port == 80;
    let mut origins: BTreeSet<String> = LOOPBACK_HOSTS.iter().map(|h| format!("http://{h}:{port}")).collect();
    origins.insert(PAGES_ORIGIN.to_string());
    if default_port {
        origins.extend(LOOPBACK_HOSTS.iter().map(|h| format!("http://{h}")));
    }
    origins.extend(config.allow_origins.iter().map(|o| normalise_origin(o)).filter(|o| !o.is_empty()));
    let hosts = loopback.then(|| {
        let mut hosts: BTreeSet<String> = LOOPBACK_HOSTS.iter().map(|h| format!("{h}:{port}")).collect();
        if default_port {
            hosts.extend(LOOPBACK_HOSTS.iter().map(|h| h.to_string()));
        }
        hosts
    });

    let state = Arc::new(State {
        reader: config.reader,
        cache: Mutex::new(Cached {
            at: None,
            body: Arc::new(Vec::new()),
            last_source: None,
        }),
        statics: config.statics,
        allowed_origins: origins,
        allowed_hosts: hosts,
        secret: config.secret,
        request_timeout: config.request_timeout,
        workers: AtomicUsize::new(0),
        stopping: AtomicBool::new(false),
        log_requests: std::env::var_os(LOG_ENV).is_some_and(|v| !v.is_empty()),
    });

    let mut addrs = Vec::new();
    let mut threads = Vec::new();
    if let Some(log) = config.csv_log {
        let state = Arc::clone(&state);
        threads.push(thread::spawn(move || csv_loop(&state, log)));
    }
    for l in listeners {
        addrs.push(l.local_addr().map_err(|error| StartError::Bind {
            addr: SocketAddr::new(v4, port),
            error,
        })?);
        let state = Arc::clone(&state);
        threads.push(thread::spawn(move || accept_loop(l, state)));
    }
    Ok(Bridge { state, addrs, threads })
}

/// Read the device at the log's interval and append a row, until shutdown
/// or the first write error.
fn csv_loop(state: &State, mut log: CsvLog) {
    while !state.stopping.load(Ordering::SeqCst) {
        let wait = log.due_in();
        if !wait.is_zero() {
            thread::sleep(wait.min(Duration::from_millis(250)));
            continue;
        }
        let data = catch_unwind(AssertUnwindSafe(|| state.reader.read()))
            .unwrap_or_else(|_| Readings::problem(state.reader.source().as_str(), "Reader error", "the reader failed unexpectedly"));
        if let Err(e) = log.record(&data) {
            println!("csv log: cannot write {}: {e}; logging stopped", log.path().display());
            return;
        }
    }
}

/// Holds one of the [`MAX_WORKERS`] slots.
struct Slot(Arc<State>);

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.workers.fetch_sub(1, Ordering::SeqCst);
    }
}

fn accept_loop(listener: TcpListener, state: Arc<State>) {
    for conn in listener.incoming() {
        if state.stopping.load(Ordering::SeqCst) {
            return;
        }
        let Ok(stream) = conn else {
            // Out of descriptors or similar: do not spin.
            thread::sleep(Duration::from_millis(50));
            continue;
        };
        // Cap concurrent workers: a flood of half-open connections cannot
        // spawn threads without limit. Over the cap, the connection is
        // simply closed.
        if state.workers.fetch_add(1, Ordering::SeqCst) >= MAX_WORKERS {
            state.workers.fetch_sub(1, Ordering::SeqCst);
            continue;
        }
        let slot = Slot(Arc::clone(&state));
        let spawned = thread::Builder::new()
            .name("wireview-worker".into())
            .stack_size(256 * 1024)
            .spawn(move || {
                let slot = slot;
                serve(&slot.0, stream);
            });
        // On failure the closure, and with it the slot and the stream, is dropped.
        drop(spawned);
    }
}

fn serve(state: &State, mut stream: TcpStream) {
    let deadline = Instant::now() + state.request_timeout;
    let _ = stream.set_nodelay(true);
    let (line, response) = match read_request(&mut stream, state.request_timeout) {
        Ok(req) => {
            let line = format!("{} {}", req.method, req.path);
            (
                line,
                catch_unwind(AssertUnwindSafe(|| respond(state, &req))).unwrap_or_else(|_| Response::error(500, "internal error")),
            )
        }
        Err(ReadError::Gone) => return,
        Err(ReadError::Malformed(why)) => ("(malformed)".to_string(), Response::error(400, why)),
        Err(ReadError::TooLarge) => ("(oversized)".to_string(), Response::error(431, "request head too large")),
    };
    if state.log_requests {
        let peer = stream.peer_addr().map_or("?".to_string(), |a| a.to_string());
        eprintln!("{peer} \"{line}\" {}", response.status);
    }
    let _ = response.write_to(&mut stream, &format!("WireViewBridge/{VERSION}"), deadline);
}

/// The request's Origin, normalised; `Err` when it was sent twice.
fn origin(req: &Request) -> Result<Option<String>, Repeated> {
    Ok(req.header("Origin")?.map(normalise_origin))
}

fn gate(state: &State, req: &Request) -> Result<Option<String>, Response> {
    // The Host header names this machine (blocks DNS rebinding).
    if let Some(hosts) = &state.allowed_hosts {
        let host = req
            .header("Host")
            .map_err(|Repeated| Response::error(400, "repeated Host header"))?;
        let host = host.unwrap_or_default().trim().to_ascii_lowercase();
        if !hosts.contains(&host) {
            return Err(Response::error(403, "unexpected Host header"));
        }
    }
    // No Origin (same-origin page, curl, the Nexus daemon) or a known one.
    let origin = origin(req).map_err(|Repeated| Response::error(400, "repeated Origin header"))?;
    match origin {
        Some(o) if !state.allowed_origins.contains(&o) => Err(Response::error(403, "origin not allowed")),
        o => Ok(o),
    }
}

fn with_cors(mut r: Response, origin: Option<&str>) -> Response {
    if let Some(o) = origin {
        // Only ever reached for an allowed origin.
        r.header("Access-Control-Allow-Origin", o);
        r.header("Vary", "Origin");
        r.header("Access-Control-Allow-Methods", "GET, OPTIONS");
        r.header("Access-Control-Allow-Headers", "*");
        r.header("Access-Control-Allow-Private-Network", "true");
    }
    r.header("Cache-Control", "no-store");
    r
}

fn json(state: &State, body: Vec<u8>, nonce: Option<&str>, origin: Option<&str>) -> Response {
    let mut r = Response::new(200);
    r.header("Content-Type", "application/json");
    if let (Some(nonce), Some(secret)) = (nonce.filter(|n| valid_nonce(n)), &state.secret) {
        r.header(AUTH_HEADER, bridge_sign(secret, nonce, &body));
    }
    r.body = body;
    with_cors(r, origin)
}

fn respond(state: &State, req: &Request) -> Response {
    if !matches!(req.method.as_str(), "GET" | "OPTIONS") {
        return Response::error(501, "unsupported method");
    }
    let origin = match gate(state, req) {
        Ok(o) => o,
        Err(refusal) => return refusal,
    };
    let origin = origin.as_deref();
    if req.method == "OPTIONS" {
        return with_cors(Response::new(204), origin);
    }
    match req.path.as_str() {
        "/api/wireview" | "/api/wireview/" => json(state, readings_json(state).to_vec(), req.query_param("nonce"), origin),
        "/api/health" => json(state, br#"{"ok":true}"#.to_vec(), None, origin),
        path => match state.statics.get(path) {
            Found::File { content_type, data } => {
                let mut r = Response::new(200);
                r.header("Content-Type", content_type);
                r.body = data;
                with_cors(r, origin)
            }
            Found::NotFound => Response::error(404, "not found"),
            Found::Forbidden => Response::error(403, "outside the served directory"),
            Found::TooLarge => Response::error(413, "file too large"),
        },
    }
}

/// The current readings as JSON, read at most once per [`CACHE`] interval.
fn readings_json(state: &State) -> Arc<Vec<u8>> {
    let mut cache = state.cache.lock().unwrap_or_else(|e| e.into_inner());
    if cache.at.is_some_and(|at| at.elapsed() <= CACHE) {
        return Arc::clone(&cache.body);
    }
    // A reader bug must not take the server down.
    let mut data = catch_unwind(AssertUnwindSafe(|| state.reader.read())).unwrap_or_else(|_| {
        let mut r = Readings::problem(state.reader.source().as_str(), "Reader error", "the reader failed unexpectedly");
        r.error = r.hint.clone();
        r
    });
    if let Some(dev) = data.device.as_mut() {
        dev.uid = None; // never served
    }
    let src = data.describe_source();
    if cache.last_source.as_deref() != Some(&src) {
        println!("readings: {src}");
        cache.last_source = Some(src);
    }
    data.served_at = Some(unix_time());
    cache.body = Arc::new(serde_json::to_vec(&data).unwrap_or_else(|_| br#"{"ok":false,"status":"Reader error"}"#.to_vec()));
    cache.at = Some(Instant::now());
    Arc::clone(&cache.body)
}
