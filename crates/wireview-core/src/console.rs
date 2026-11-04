//! Console handling for programs built as Windows GUI-subsystem executables.
//!
//! The daemons start at logon from a Scheduled Task and must not open a
//! console window, so they are linked for the GUI subsystem. Started from a
//! terminal they should still print (`--help`, `--version`, log lines), so
//! [`attach`] connects to the parent's console when there is one.
//!
//! With no console at all - the Task Scheduler case - stdout and stderr used
//! to be null handles and every `println!` vanished: a daemon that could not
//! open its port failed silently. In that case [`attach`] now redirects both
//! to `%LOCALAPPDATA%\wireview\<exe>.log`, one file per daemon, so the
//! existing messages survive a headless start. The daemons call
//! [`maintain`] from their loops so a long run cannot grow the file without
//! bound. With stdout a real file, a failed write makes `println!` panic:
//! the daemons print through [`say!`](crate::say) instead.

/// Attach to the parent process's console, if any. Redirected output
/// (`> file`, a pipe) is left alone. Without a console on Windows, stdout
/// and stderr go to a per-executable log file. Does nothing outside Windows.
pub fn attach() {
    #[cfg(windows)]
    windows::attach();
}

/// Rotate the fallback log once it has grown past the cap while the daemon
/// runs. Cheap to call often (the file is looked at once a minute at most);
/// does nothing when output is not the log file, or outside Windows.
pub fn maintain() {
    #[cfg(windows)]
    windows::maintain();
}

/// The fallback log is rotated to `.old.log` when it reaches this size, at
/// a start or by [`maintain`]; with the daemons' throttled messages that is
/// months of restarts, and two files at most are ever kept.
#[cfg(windows)]
const MAX_LOG_BYTES: u64 = 1024 * 1024;

/// Where the no-console fallback log goes: `LOCALAPPDATA\wireview\<exe>.log`,
/// falling back to the temp dir when LOCALAPPDATA is unset. Split out and
/// environment-injected so it is testable without Windows.
#[cfg(any(windows, test))]
fn log_path_in(base: Option<std::path::PathBuf>, exe: &str) -> std::path::PathBuf {
    let base = base.unwrap_or_else(std::env::temp_dir);
    base.join("wireview").join(format!("{exe}.log"))
}

#[cfg(windows)]
fn log_path() -> std::path::PathBuf {
    let base = std::env::var_os("LOCALAPPDATA").map(std::path::PathBuf::from);
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_stem().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "wireview".to_string());
    log_path_in(base, &exe)
}

#[cfg(test)]
mod tests {
    use super::log_path_in;
    use std::path::PathBuf;

    #[test]
    fn log_path_uses_localappdata_and_exe_name() {
        // No separator literals: CI runs this on Linux too, where join()
        // appends '/' while the Windows-style base keeps '\'.
        let base = PathBuf::from("C:\\Users\\me\\AppData\\Local");
        let p = log_path_in(Some(base.clone()), "wireview-bridge");
        assert_eq!(p.file_name().unwrap(), "wireview-bridge.log");
        assert_eq!(p.parent().unwrap().file_name().unwrap(), "wireview");
        assert!(p.starts_with(&base));
    }

    #[test]
    fn log_path_falls_back_to_temp_when_unset() {
        let p = log_path_in(None, "wireview-nexus");
        assert_eq!(p, std::env::temp_dir().join("wireview").join("wireview-nexus.log"));
    }
}

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, FILE_APPEND_DATA, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_ALWAYS, OPEN_EXISTING,
    };
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };

    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn usable(h: HANDLE) -> bool {
        !h.is_null() && h != INVALID_HANDLE_VALUE
    }

    pub fn attach() {
        // SAFETY: plain Win32 calls; every pointer passed is valid for the call.
        unsafe {
            if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
                // No parent console (started by the Task Scheduler): stdout
                // and stderr are null handles, so fall back to a log file.
                redirect_to_file();
                return;
            }
            let name: Vec<u16> = "CONOUT$".encode_utf16().chain(std::iter::once(0)).collect();
            for which in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
                let which: STD_HANDLE = which;
                if usable(GetStdHandle(which)) {
                    continue; // redirected by the caller
                }
                let h = CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    std::ptr::null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                );
                if usable(h) {
                    SetStdHandle(which, h);
                }
            }
        }
    }

    /// Set when stdout/stderr were pointed at the log file.
    static REDIRECTED: AtomicBool = AtomicBool::new(false);
    /// The handles this module set, as integers (a HANDLE is a raw pointer
    /// and cannot sit in a static); 0 = that stream was not redirected.
    static OURS: Mutex<[isize; 2]> = Mutex::new([0; 2]);
    static LAST_CHECK: Mutex<Option<Instant>> = Mutex::new(None);
    const CHECK_EVERY: Duration = Duration::from_secs(60);
    const STREAMS: [STD_HANDLE; 2] = [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];

    /// Open `path` for appending (`FILE_APPEND_DATA`: every write lands at
    /// the end, so two instances interleave by line without clobbering).
    ///
    /// SAFETY: plain Win32 call; the pointer is valid for the call.
    unsafe fn open_append(path: &std::path::Path) -> HANDLE {
        let wide: Vec<u16> = path.to_string_lossy().encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            CreateFileW(
                wide.as_ptr(),
                FILE_APPEND_DATA,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                std::ptr::null(),
                OPEN_ALWAYS,
                0,
                std::ptr::null_mut(),
            )
        }
    }

    /// One dated line: the daemons' own messages carry no time, and this is
    /// what makes a restart loop legible. Not println!: stdout is a real
    /// file now, and a write error (full disk) must not panic.
    fn stamp(what: &str) {
        use std::io::Write as _;
        let _ = writeln!(
            std::io::stdout(),
            "---- {what} {} (pid {}) ----",
            crate::csvlog::LocalTime::now().iso8601(),
            std::process::id()
        );
    }

    fn rotate(path: &std::path::Path) -> bool {
        std::fs::metadata(path).is_ok_and(|m| m.len() >= super::MAX_LOG_BYTES)
            && std::fs::rename(path, path.with_extension("old.log")).is_ok()
    }

    /// No console exists (Task Scheduler start): append stdout/stderr to a
    /// per-executable log file so daemon messages survive the run. Runs
    /// accumulate in one file, each under a dated start line, so a restart
    /// loop stays readable; past [`super::MAX_LOG_BYTES`] the file is
    /// rotated to `.old.log` (at most two files).
    fn redirect_to_file() {
        let path = super::log_path();
        let _ = std::fs::create_dir_all(path.parent().unwrap_or_else(|| std::path::Path::new(".")));
        rotate(&path);
        let mut redirected = false;
        {
            let mut ours = OURS.lock().unwrap_or_else(|e| e.into_inner());
            // SAFETY: plain Win32 calls; every pointer passed is valid for the call.
            unsafe {
                for (i, which) in STREAMS.into_iter().enumerate() {
                    if usable(GetStdHandle(which)) {
                        continue; // redirected by the caller
                    }
                    let h = open_append(&path);
                    if usable(h) {
                        SetStdHandle(which, h);
                        ours[i] = h as isize;
                        redirected = true;
                    }
                }
            }
        }
        if redirected {
            REDIRECTED.store(true, Ordering::Release);
            stamp("started");
        }
    }

    /// Rotate a log that grew past the cap while running: rename it and
    /// point the streams at a fresh file. The old handle is left open on
    /// purpose: a writer may have fetched it a moment ago, and a closed
    /// handle value could be reused for another file. One leaked handle
    /// per MiB of log is nothing.
    pub fn maintain() {
        if !REDIRECTED.load(Ordering::Acquire) {
            return;
        }
        {
            let mut last = LAST_CHECK.lock().unwrap_or_else(|e| e.into_inner());
            if last.is_some_and(|t| t.elapsed() < CHECK_EVERY) {
                return;
            }
            *last = Some(Instant::now());
        }
        let path = super::log_path();
        if !rotate(&path) {
            return;
        }
        {
            let mut ours = OURS.lock().unwrap_or_else(|e| e.into_inner());
            // SAFETY: plain Win32 calls; every pointer passed is valid for the call.
            unsafe {
                for (i, which) in STREAMS.into_iter().enumerate() {
                    if ours[i] == 0 {
                        continue;
                    }
                    let h = open_append(&path);
                    if usable(h) {
                        SetStdHandle(which, h);
                        ours[i] = h as isize;
                    }
                }
            }
        }
        stamp("continued");
    }
}
