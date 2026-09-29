//! Print one reading as JSON: `cargo run --example read -- [auto|serial|hwinfo|bridge] [bridge-url]`.

use wireview_core::{DEFAULT_BRIDGE_URL, Reader, Source};

fn main() {
    let mut args = std::env::args().skip(1);
    let source: Source = match args.next().as_deref().unwrap_or("auto").parse() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e}");
            std::process::exit(2);
        }
    };
    let url = args.next().unwrap_or_else(|| DEFAULT_BRIDGE_URL.to_string());
    let reader = Reader::new(source, None, Some(url));
    println!("{}", serde_json::to_string_pretty(&reader.read()).expect("readings serialise"));
    if let Some(e) = reader.bridge_error() {
        eprintln!("bridge: {e}");
    }
}
