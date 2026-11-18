//! Small Windows platform layer: read-only process detection and single-
//! instance locking. Non-Windows builds get inert stubs so the core compiles
//! and tests everywhere.
//!
//! Process detection enumerates names only (`CreateToolhelp32Snapshot`); it
//! never opens a handle to the game — see the `eac-safety` guarantee.

/// True if a process with the given executable name (e.g. `eldenring.exe`) is
/// currently running. Case-insensitive.
///
/// `app_process` lists executable names through one Toolhelp snapshot and
/// never opens a handle to any process, which keeps this outside Easy
/// Anti-Cheat's view.
#[must_use]
pub fn process_running(exe_name: &str) -> bool {
    app_process::is_running(exe_name)
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

/// A single-instance lock. On Windows this is a named mutex whose lifetime is
/// tied to the returned guard; dropping it releases the lock.
pub struct SingleInstance {
    #[cfg(windows)]
    handle: windows_sys::Win32::Foundation::HANDLE,
}

/// Why [`SingleInstance::acquire`] returned no lock.
#[derive(Debug)]
pub enum LockError {
    /// Another process holds the lock.
    Held,
    /// The lock could not be created, so whether another process holds it is
    /// unknown.
    Failed(std::io::Error),
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Held => f.write_str("another process holds the lock"),
            LockError::Failed(error) => write!(f, "the lock could not be created: {error}"),
        }
    }
}

impl std::error::Error for LockError {}

impl SingleInstance {
    /// Acquire the named lock.
    ///
    /// # Errors
    ///
    /// Returns [`LockError::Held`] if another process already holds it (a
    /// monitor is already running), or [`LockError::Failed`] if Windows could
    /// not create it.
    #[cfg(windows)]
    pub fn acquire(name: &str) -> Result<Self, LockError> {
        use windows_sys::Win32::Foundation::{CloseHandle, ERROR_ALREADY_EXISTS};
        use windows_sys::Win32::System::Threading::CreateMutexW;

        let wide: Vec<u16> = format!("Local\\{name}")
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: creating a named mutex with a valid null-terminated name and
        // no security attributes.
        let handle = unsafe { CreateMutexW(std::ptr::null(), 1, wide.as_ptr()) };
        // Read the last error at once, before anything else can change it.
        let error = std::io::Error::last_os_error();
        if handle.is_null() {
            return Err(LockError::Failed(error));
        }
        if error.raw_os_error() == i32::try_from(ERROR_ALREADY_EXISTS).ok() {
            // SAFETY: `handle` is a valid handle this function owns and no
            // longer uses.
            unsafe { CloseHandle(handle) };
            return Err(LockError::Held);
        }
        Ok(Self { handle })
    }

    /// Acquire the named lock. Off Windows there is no lock to take.
    ///
    /// # Errors
    ///
    /// Never fails off Windows.
    #[cfg(not(windows))]
    pub fn acquire(_name: &str) -> Result<Self, LockError> {
        Ok(Self {})
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
