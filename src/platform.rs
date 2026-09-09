//! Small Windows platform layer: read-only process detection and single-
//! instance locking. Non-Windows builds get inert stubs so the core compiles
//! and tests everywhere.
//!
//! Process detection enumerates names only (`CreateToolhelp32Snapshot`); it
//! never opens a handle to the game — see the `eac-safety` guarantee.

/// True if a process with the given executable name (e.g. `eldenring.exe`) is
/// currently running. Case-insensitive.
#[cfg(windows)]
#[must_use]
#[expect(
    clippy::multiple_unsafe_ops_per_block,
    reason = "Toolhelp enumeration owns one snapshot handle for the complete iteration"
)]
pub fn process_running(exe_name: &str) -> bool {
    use std::mem::{size_of, zeroed};
    use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    // SAFETY: standard Toolhelp enumeration. We only read process names; the
    // snapshot handle is closed before returning.
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snapshot == INVALID_HANDLE_VALUE {
            return false;
        }
        let mut entry: PROCESSENTRY32W = zeroed();
        entry.dwSize = u32::try_from(size_of::<PROCESSENTRY32W>()).unwrap_or_default();

        let mut found = false;
        if Process32FirstW(snapshot, &raw mut entry) != 0 {
            loop {
                if wide_eq_ignore_case(&entry.szExeFile, exe_name) {
                    found = true;
                    break;
                }
                if Process32NextW(snapshot, &raw mut entry) == 0 {
                    break;
                }
            }
        }
        CloseHandle(snapshot);
        found
    }
}

#[cfg(not(windows))]
#[must_use]
pub fn process_running(_exe_name: &str) -> bool {
    false
}

#[cfg(windows)]
fn wide_eq_ignore_case(wide: &[u16], name: &str) -> bool {
    let end = wide.iter().position(|&c| c == 0).unwrap_or(wide.len());
    let s = String::from_utf16_lossy(&wide[..end]);
    s.eq_ignore_ascii_case(name)
}

/// Free bytes available to the caller on the volume containing `path`.
/// Returns `None` if it cannot be determined.
#[cfg(windows)]
#[must_use]
pub fn free_space(path: &std::path::Path) -> Option<u64> {
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut free: u64 = 0;
    // SAFETY: valid null-terminated path; out-params are owned locals.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &raw mut free,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    (ok != 0).then_some(free)
}

#[cfg(not(windows))]
#[must_use]
pub fn free_space(_path: &std::path::Path) -> Option<u64> {
    None
}

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

/// Open a folder in the system file manager (Explorer on Windows). Best-effort.
///
/// On Windows this asks the shell rather than spawning `explorer.exe`.
/// Explorer hands the path to the running shell and exits, so the launch can
/// fail after `spawn` has already reported success and nothing ever opens.
pub fn open_folder(path: &std::path::Path) {
    #[cfg(windows)]
    {
        use windows_sys::Win32::UI::Shell::ShellExecuteW;
        use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

        /// Values at or below this mean `ShellExecuteW` failed. Above it, the
        /// return value is a legacy instance handle.
        const SHELL_EXECUTE_MIN_SUCCESS: usize = 32;

        let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
        if wide.contains(&0) {
            return;
        }
        wide.push(0);
        let verb: Vec<u16> = "open\0".encode_utf16().collect();

        // SAFETY: both strings are NUL-terminated and live for the duration of
        // this synchronous call, and every other argument is the documented
        // null. The caller is the egui window thread, where winit has already
        // initialized COM for the shell extensions Explorer loads.
        let instance = unsafe {
            ShellExecuteW(
                std::ptr::null_mut(),
                verb.as_ptr(),
                wide.as_ptr(),
                std::ptr::null(),
                std::ptr::null(),
                SW_SHOWNORMAL,
            )
        };
        if (instance as usize) <= SHELL_EXECUTE_MIN_SUCCESS {
            tracing::warn!(code = instance as usize, "failed to open the folder");
        }
    }
    #[cfg(not(windows))]
    let _ = std::process::Command::new("xdg-open").arg(path).spawn();
}

/// A single-instance lock. On Windows this is a named mutex whose lifetime is
/// tied to the returned guard; dropping it releases the lock.
pub struct SingleInstance {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
}

impl SingleInstance {
    /// Acquire the named lock. Returns `None` if another process already holds
    /// it (i.e. a monitor is already running).
    #[cfg(windows)]
    #[must_use]
    #[expect(
        clippy::multiple_unsafe_ops_per_block,
        reason = "mutex creation, last-error inspection, and duplicate-handle cleanup are one transaction"
    )]
    pub fn acquire(name: &str) -> Option<Self> {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError};
        use windows_sys::Win32::System::Threading::CreateMutexW;

        let wide: Vec<u16> = format!("Local\\{name}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: creating a named mutex with a valid null-terminated name.
        unsafe {
            let handle = CreateMutexW(std::ptr::null(), 1, wide.as_ptr());
            if handle.is_null() {
                return None;
            }
            if GetLastError() == ERROR_ALREADY_EXISTS {
                CloseHandle(handle);
                return None;
            }
            Some(Self { handle })
        }
    }

    #[cfg(not(windows))]
    #[must_use]
    pub fn acquire(_name: &str) -> Option<Self> {
        Some(Self {})
    }
}

#[cfg(windows)]
impl Drop for SingleInstance {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::CloseHandle;
        // SAFETY: `handle` came from CreateMutexW and is owned by this guard.
        unsafe {
            CloseHandle(self.handle);
        }
    }
}
