//! Console handling for programs built as Windows GUI-subsystem executables.
//!
//! The daemons start at logon from a Scheduled Task and must not open a
//! console window, so they are linked for the GUI subsystem. Started from a
//! terminal they should still print (`--help`, `--version`, log lines), so
//! [`attach`] connects to the parent's console when there is one.

/// Attach to the parent process's console, if any. Redirected output
/// (`> file`, a pipe) is left alone. Does nothing outside Windows.
pub fn attach() {
    #[cfg(windows)]
    windows::attach();
}

#[cfg(windows)]
mod windows {
    use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, HANDLE, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING};
    use windows_sys::Win32::System::Console::{
        ATTACH_PARENT_PROCESS, AttachConsole, GetStdHandle, STD_ERROR_HANDLE, STD_HANDLE, STD_OUTPUT_HANDLE, SetStdHandle,
    };

    fn usable(h: HANDLE) -> bool {
        !h.is_null() && h != INVALID_HANDLE_VALUE
    }

    pub fn attach() {
        // SAFETY: plain Win32 calls; every pointer passed is valid for the call.
        unsafe {
            if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
                return; // no parent console (started by the Task Scheduler)
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
}
