//! `wireview-bridge`: see the library documentation.
//!
//! ```text
//! wireview-bridge                  # port 8765, serves the widget pages too
//! wireview-bridge --port 9000
//! wireview-bridge --no-static      # JSON only
//! wireview-bridge --source hwinfo
//! wireview-bridge --allow-origin https://example.github.io   # another widget host
//! wireview-bridge --csv-log C:\WireView\logs            # also log a CSV row a minute
//! ```

// Started at logon by a Scheduled Task: no console window.
#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use clap::Parser;
use wireview_bridge::{Config, DEFAULT_PORT, PAGES_ORIGIN, Statics, VERSION, start};
use wireview_core::auth::SecretStore;
use wireview_core::csvlog::{self, CsvLog};
use wireview_core::{Reader, Source};

fn source(s: &str) -> Result<Source, String> {
    match s.parse()? {
        Source::Bridge => Err("the bridge cannot read from a bridge".into()),
        s => Ok(s),
    }
}

#[derive(Parser)]
#[command(name = "wireview-bridge", version = VERSION, about = "WireView Pro II localhost bridge")]
struct Args {
    #[arg(long, default_value_t = DEFAULT_PORT, value_parser = clap::value_parser!(u16).range(1..))]
    port: u16,

    /// Interface to bind (default localhost only; anything else exposes the readings to that network)
    #[arg(long, default_value = "127.0.0.1")]
    bind: String,

    /// Extra web origin allowed to read the API, e.g. https://you.github.io
    /// (loopback and the GitHub Pages copy are always allowed; repeatable)
    #[arg(long, value_name = "ORIGIN")]
    allow_origin: Vec<String>,

    /// Do not serve the widget pages, JSON only
    #[arg(long, conflicts_with = "static_dir")]
    no_static: bool,

    /// Serve the widget pages from this directory instead of the built-in copy
    #[arg(long, value_name = "DIR")]
    static_dir: Option<PathBuf>,

    /// serial (direct USB), hwinfo, or auto
    #[arg(long, default_value = "auto", value_parser = source, value_name = "auto|serial|hwinfo")]
    source: Source,

    /// WireView COM port (default: auto-detect)
    #[arg(long, value_name = "COMx")]
    serial_port: Option<String>,

    /// Also write the readings to a new log-<date>-<time>.csv in this directory,
    /// in the format the Thermal Grizzly WireView app exports (off by default)
    #[arg(long, value_name = "DIR")]
    csv_log: Option<PathBuf>,

    /// Seconds between CSV rows (1-86400)
    #[arg(long, value_name = "SECONDS", default_value = "60", value_parser = csvlog::parse_interval, requires = "csv_log")]
    csv_interval: Duration,
}

fn main() -> ExitCode {
    wireview_core::console::attach();
    let args = Args::parse();

    let statics = match (&args.static_dir, args.no_static) {
        (_, true) => Statics::None,
        (Some(dir), _) if !dir.is_dir() => {
            eprintln!("--static-dir {}: not a directory", dir.display());
            return ExitCode::from(2);
        }
        (Some(dir), _) => Statics::Dir(dir.clone()),
        (None, _) => Statics::Embedded,
    };

    let secrets = SecretStore::default();
    let secret = secrets.get(true);
    if secret.is_none() {
        println!(
            "WARNING: cannot create the bridge secret at {}; other programs will not trust this bridge \
             and will keep the COM port to themselves.",
            secrets.path().display()
        );
    }

    let csv_log = match &args.csv_log {
        None => None,
        Some(dir) => match CsvLog::create(dir, args.csv_interval) {
            Ok(log) => Some(log),
            Err(e) => {
                eprintln!("--csv-log {}: cannot create the log file: {e}", dir.display());
                return ExitCode::from(2);
            }
        },
    };

    let mut config = Config::new(Reader::new(args.source, args.serial_port, None));
    config.port = args.port;
    config.bind = args.bind.clone();
    config.allow_origins = args.allow_origin;
    config.statics = statics.clone();
    config.secret = secret;
    if let Some(log) = &csv_log {
        println!("CSV log: {} (a row every {} s)", log.path().display(), log.interval().as_secs());
    }
    config.csv_log = csv_log;

    let bridge = match start(config) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::FAILURE;
        }
    };
    if !matches!(args.bind.as_str(), "127.0.0.1" | "localhost" | "::1" | "[::1]") {
        println!(
            "WARNING: --bind {} makes the readings and widgets reachable from that network; \
             the Host check is off. Prefer the default loopback bind.",
            args.bind
        );
    }
    for addr in bridge.addrs() {
        println!("WireView bridge {VERSION} listening on http://{addr}/api/wireview");
    }
    match &statics {
        Statics::None => {}
        Statics::Embedded => println!(
            "Serving the built-in widgets at http://localhost:{}/ (also hosted at {PAGES_ORIGIN})",
            args.port
        ),
        Statics::Dir(d) => println!("Serving widgets from {} at http://localhost:{}/", d.display(), args.port),
    }
    bridge.wait();
    ExitCode::SUCCESS
}
