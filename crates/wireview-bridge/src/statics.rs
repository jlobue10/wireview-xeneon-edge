//! The widget pages: compiled into the executable, or read from a directory
//! when `--static-dir` asks for that.

use std::fs;
use std::io::Read;
use std::path::PathBuf;

mod embedded {
    include!(concat!(env!("OUT_DIR"), "/embedded.rs"));
}

pub const MAX_STATIC_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Statics {
    /// JSON only.
    None,
    /// The copy of `docs/` built into the executable.
    Embedded,
    /// A directory on disk.
    Dir(PathBuf),
}

#[derive(Debug, PartialEq, Eq)]
pub enum Found {
    File {
        content_type: &'static str,
        data: Vec<u8>,
    },
    NotFound,
    /// The path resolves outside the root.
    Forbidden,
    TooLarge,
}

pub fn content_type(path: &str) -> &'static str {
    let ext = path.rsplit_once('.').map_or("", |(_, e)| e).to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "txt" => "text/plain; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "ico" => "image/x-icon",
        "woff2" => "font/woff2",
        _ => "application/octet-stream",
    }
}

impl Statics {
    /// The file for request path `path` (as sent, starting with `/`).
    pub fn get(&self, path: &str) -> Found {
        match self {
            Statics::None => Found::NotFound,
            Statics::Embedded => embedded_file(path),
            Statics::Dir(root) => disk_file(root, path),
        }
    }
}

fn embedded_file(path: &str) -> Found {
    let rel = path.trim_matches('/');
    let lookup = |name: &str| embedded::FILES.iter().find(|(n, _)| *n == name);
    let index = if rel.is_empty() {
        "index.html".to_string()
    } else {
        format!("{rel}/index.html")
    };
    // A name is matched whole against a fixed table, so "..", "//" and the
    // like simply match nothing.
    let hit = if path.ends_with('/') || rel.is_empty() {
        lookup(&index)
    } else {
        lookup(rel).or_else(|| lookup(&index))
    };
    match hit {
        Some((name, data)) => Found::File {
            content_type: content_type(name),
            data: data.to_vec(),
        },
        None => Found::NotFound,
    }
}

fn disk_file(root: &std::path::Path, path: &str) -> Found {
    let rel = path.trim_start_matches('/');
    if rel.contains('\0') {
        return Found::NotFound;
    }
    let mut target = if rel.is_empty() { root.to_path_buf() } else { root.join(rel) };
    if target.is_dir() {
        target = target.join("index.html");
    }
    // Resolve the *final* file (after the index was appended) and require it
    // to live under the root, so neither ".." nor a symlink escapes.
    let Ok(root) = root.canonicalize() else { return Found::NotFound };
    let Ok(target) = target.canonicalize() else {
        return Found::NotFound;
    };
    if target == root || !target.starts_with(&root) {
        return Found::Forbidden;
    }
    let Ok(file) = fs::File::open(&target) else {
        return Found::NotFound;
    };
    match file.metadata() {
        Ok(m) if !m.is_file() => return Found::NotFound,
        Ok(m) if m.len() > MAX_STATIC_BYTES => return Found::TooLarge,
        Ok(_) => {}
        Err(_) => return Found::NotFound,
    }
    let mut data = Vec::new();
    // The size may change between the check and the read; cap the read too.
    match file.take(MAX_STATIC_BYTES + 1).read_to_end(&mut data) {
        Ok(n) if n as u64 > MAX_STATIC_BYTES => Found::TooLarge,
        Ok(_) => Found::File {
            content_type: content_type(&target.to_string_lossy()),
            data,
        },
        Err(_) => Found::NotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_html(f: Found) -> bool {
        matches!(f, Found::File { content_type, ref data } if content_type.starts_with("text/html") && !data.is_empty())
    }

    #[test]
    fn embedded_pages_are_served() {
        let s = Statics::Embedded;
        for p in [
            "/",
            "/index.html",
            "/per-wire/",
            "/per-wire",
            "/per-wire/index.html",
            "/total-current/",
            "/total-power/",
        ] {
            assert!(is_html(s.get(p)), "{p}");
        }
        assert!(matches!(s.get("/common/wireview.js"), Found::File { content_type, .. } if content_type.starts_with("text/javascript")));
        assert!(matches!(s.get("/common/style.css"), Found::File { content_type, .. } if content_type.starts_with("text/css")));
    }

    #[test]
    fn embedded_lookups_cannot_wander() {
        let s = Statics::Embedded;
        for p in [
            "/nope",
            "/../Cargo.toml",
            "/per-wire/../index.html",
            "/common/",
            "/.nojekyll",
            "/%2e%2e/",
            "/C:/Windows/win.ini",
        ] {
            assert!(s.get(p) == Found::NotFound, "{p}");
        }
        assert_eq!(Statics::None.get("/"), Found::NotFound);
    }

    #[test]
    fn disk_root_contains_the_request() {
        let base = std::env::temp_dir().join(format!("wireview-statics-{}", std::process::id()));
        let root = base.join("docs");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::create_dir_all(root.join("nested")).unwrap();
        fs::write(root.join("index.html"), "<p>root</p>").unwrap();
        fs::write(root.join("sub").join("index.html"), "<p>sub</p>").unwrap();
        fs::write(base.join("OUTSIDE.txt"), "outside").unwrap();
        let s = Statics::Dir(root.clone());

        assert!(is_html(s.get("/")));
        assert!(is_html(s.get("/sub/")));
        assert!(is_html(s.get("/sub")));
        assert_eq!(s.get("/missing.html"), Found::NotFound);
        assert_eq!(s.get("/../OUTSIDE.txt"), Found::Forbidden);
        assert_eq!(s.get("/sub/../../OUTSIDE.txt"), Found::Forbidden);

        #[cfg(unix)]
        {
            // C5 / WV-06: an index.html that is a symlink out of the root.
            std::os::unix::fs::symlink(base.join("OUTSIDE.txt"), root.join("nested").join("index.html")).unwrap();
            std::os::unix::fs::symlink(base.join("OUTSIDE.txt"), root.join("link.txt")).unwrap();
            assert_eq!(s.get("/nested/"), Found::Forbidden);
            assert_eq!(s.get("/link.txt"), Found::Forbidden);
        }

        let big = fs::File::create(root.join("big.bin")).unwrap();
        big.set_len(MAX_STATIC_BYTES + 1).unwrap();
        assert_eq!(s.get("/big.bin"), Found::TooLarge);
        fs::remove_dir_all(base).unwrap();
    }
}
