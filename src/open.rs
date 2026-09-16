//! Files opened via the OS (macOS Open With / Finder double-click).
//!
//! LaunchServices hands an opened document to the app as an Apple Event
//! on the main thread — never as argv — so the paths arrive through the
//! `open_shim.m` application delegate mid-run-loop. They buffer here and
//! `Runner::about_to_wait` drains them, flushing the whole batch through
//! `files_dropped(paths, false)`: the drag-in path already does exactly
//! what Open With should (survivors fill the next free slots). Always an
//! ADD, never the ⌘-replace — a ⌘ held while picking a menu item must not
//! wipe the set. The notify hook covers the running-app case: an open
//! landing while the loop sleeps on the idle tick still redraws promptly.

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{CStr, OsStr, c_char};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    use crate::player::Notify;

    static OPENED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    static NOTIFY: OnceLock<Notify> = OnceLock::new();

    unsafe extern "C" {
        fn ab_install_open_handler(cb: extern "C" fn(*const c_char)) -> std::ffi::c_int;
    }

    extern "C" fn on_open(path: *const c_char) {
        if path.is_null() {
            return;
        }
        let bytes = unsafe { CStr::from_ptr(path) }.to_bytes();
        let path = PathBuf::from(OsStr::from_bytes(bytes));
        log::debug!("open-with: {}", path.display());
        OPENED.lock().unwrap().push(path);
        if let Some(n) = NOTIFY.get() {
            n();
        }
    }

    /// Graft the open-files handler onto winit's application delegate
    /// (never REPLACE it — winit 0.30 panics on a foreign delegate; see
    /// open_shim.m). Main thread, after winit's `EventLoop::new` and
    /// before the loop runs — the launch-time open event fires at startup.
    pub fn install(notify: Notify) {
        let _ = NOTIFY.set(notify);
        match unsafe { ab_install_open_handler(on_open) } {
            1 => {}
            0 => log::warn!("open-with: no application delegate to graft onto"),
            _ => log::warn!(
                "open-with: winit's delegate now implements application:openURLs: itself; \
                 opened files will not reach the window — rework open_shim.m"
            ),
        }
    }

    /// Take every path the OS has asked us to open since the last drain.
    pub fn drain() -> Vec<PathBuf> {
        std::mem::take(&mut *OPENED.lock().unwrap())
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use std::path::PathBuf;

    pub fn install(_notify: crate::player::Notify) {}

    pub fn drain() -> Vec<PathBuf> {
        Vec::new()
    }
}

pub use imp::{drain, install};
