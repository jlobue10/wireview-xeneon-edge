//! Embed the widget pages (`docs/`) in the executable, so the bridge is one
//! file and serving them never touches the file system.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::{env, fs};

fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf)>) {
    let mut entries: Vec<_> = fs::read_dir(dir).expect("docs/ is readable").flatten().collect();
    entries.sort_by_key(|e| e.file_name());
    for e in entries {
        let name = e.file_name().to_string_lossy().into_owned();
        let kind = e.file_type().expect("file type");
        if name.starts_with('.') || kind.is_symlink() {
            continue;
        }
        let rel = if rel.is_empty() { name } else { format!("{rel}/{name}") };
        if kind.is_dir() {
            walk(&e.path(), &rel, out);
        } else if kind.is_file() {
            out.push((rel, e.path()));
        }
    }
}

fn main() {
    let docs = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("../../docs");
    let docs = docs.canonicalize().expect("docs/ exists next to crates/");
    println!("cargo:rerun-if-changed={}", docs.display());
    let mut files = Vec::new();
    walk(&docs, "", &mut files);
    let mut src = String::from("pub static FILES: &[(&str, &[u8])] = &[\n");
    for (rel, path) in &files {
        println!("cargo:rerun-if-changed={}", path.display());
        writeln!(src, "    ({rel:?}, include_bytes!({:?})),", path.display().to_string()).unwrap();
    }
    src.push_str("];\n");
    fs::write(Path::new(&env::var("OUT_DIR").unwrap()).join("embedded.rs"), src).unwrap();
}
