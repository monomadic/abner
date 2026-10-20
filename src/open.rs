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
//!
//! The native File / View / Frame menus live here too (`open_shim.m` builds
//! them): a menu pick queues a `MenuAction` the same way, the runner applies
//! it in `about_to_wait`, and `sync_menus` re-derives every item's state
//! from `App` each turn — never from the click.

/// A pick from the native File or Frame menu. Numbers match open_shim.m.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuAction {
    /// File > Open… (⌘O): the add-sources picker.
    Open,
    /// File > Reveal in Finder (⇧⌘R): the focused clip, selected in Finder.
    Reveal,
    /// File > Export (⌘E): the timeline's export, or the crop marquee's ProRes.
    Export,
    /// File > Close (⌘W): the focused clip; the empty window quits.
    Close,
    /// Frame > Copy Frame as PNG (⌘C): the focused clip's frame on screen.
    CopyFrame,
}

impl MenuAction {
    fn from_tag(tag: i32) -> Option<Self> {
        Some(match tag {
            1 => Self::Open,
            2 => Self::Reveal,
            3 => Self::Export,
            4 => Self::Close,
            5 => Self::CopyFrame,
            _ => return None,
        })
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::ffi::{CStr, OsStr, c_char};
    use std::os::unix::ffi::OsStrExt;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};
    use std::sync::atomic::{AtomicI32, Ordering};

    use super::MenuAction;
    use crate::player::Notify;

    static OPENED: Mutex<Vec<PathBuf>> = Mutex::new(Vec::new());
    static NOTIFY: OnceLock<Notify> = OnceLock::new();
    static VIEW_REQUEST: AtomicI32 = AtomicI32::new(-1);
    static MENU_REQUESTS: Mutex<Vec<MenuAction>> = Mutex::new(Vec::new());

    unsafe extern "C" {
        fn ab_choose_files();
        fn ab_sync_menus(mode: i32, has_clips: bool, can_export: bool, view_cb: extern "C" fn(i32), menu_cb: extern "C" fn(i32));
        fn ab_copy_png(bytes: *const u8, len: usize);
        fn ab_reveal_in_finder(path: *const c_char);
        fn ab_install_open_handler(cb: extern "C" fn(*const c_char)) -> std::ffi::c_int;
    }

    extern "C" fn on_view(mode: i32) {
        if !(0..4).contains(&mode) { return; }
        VIEW_REQUEST.store(mode, Ordering::Relaxed);
        if let Some(n) = NOTIFY.get() { n(); }
    }

    pub fn take_view_request() -> Option<usize> {
        let mode = VIEW_REQUEST.swap(-1, Ordering::Relaxed);
        (mode >= 0).then_some(mode as usize)
    }

    extern "C" fn on_menu(tag: i32) {
        let Some(action) = MenuAction::from_tag(tag) else { return };
        MENU_REQUESTS.lock().unwrap().push(action);
        if let Some(n) = NOTIFY.get() { n(); }
    }

    /// Every menu pick since the last drain, oldest first.
    pub fn take_menu_requests() -> Vec<MenuAction> {
        std::mem::take(&mut *MENU_REQUESTS.lock().unwrap())
    }

    /// Main-thread only. Derive checkmarks and enabled items from App,
    /// never from menu clicks.
    pub fn sync_menus(mode: usize, has_clips: bool, can_export: bool) {
        unsafe { ab_sync_menus(mode as i32, has_clips, can_export, on_view, on_menu); }
    }

    /// Encoded PNG bytes onto the general pasteboard. Main thread.
    pub fn copy_png(bytes: &[u8]) {
        unsafe { ab_copy_png(bytes.as_ptr(), bytes.len()); }
    }

    /// Select the file in a Finder window.
    pub fn reveal(path: &std::path::Path) {
        let Ok(c) = std::ffi::CString::new(path.as_os_str().as_bytes()) else { return };
        unsafe { ab_reveal_in_finder(c.as_ptr()); }
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

    pub fn paste_text() -> Option<String> {
        unsafe extern "C" {fn ab_paste_text()->*mut c_char;fn ab_free_text(text:*mut c_char);}
        unsafe {let text=ab_paste_text();if text.is_null(){return None;}
            let value=CStr::from_ptr(text).to_string_lossy().into_owned();ab_free_text(text);Some(value)}
    }
    pub fn choose_files() { unsafe { ab_choose_files(); } }

    /// Take every path the OS has asked us to open since the last drain.
    pub fn drain() -> Vec<PathBuf> {
        std::mem::take(&mut *OPENED.lock().unwrap())
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    use std::path::PathBuf;

    pub fn paste_text()->Option<String>{None}
    pub fn choose_files() {}
    pub fn take_view_request() -> Option<usize> { None }
    pub fn take_menu_requests() -> Vec<super::MenuAction> { Vec::new() }
    pub fn sync_menus(_mode: usize, _has_clips: bool, _can_export: bool) {}
    pub fn copy_png(_bytes: &[u8]) {}
    pub fn reveal(path: &std::path::Path) {
        // No Finder: open the containing folder with the desktop's opener.
        let Some(dir) = path.parent() else { return };
        #[cfg(target_os = "linux")]
        let opener = "xdg-open";
        #[cfg(not(target_os = "linux"))]
        let opener = "explorer";
        if let Err(err) = std::process::Command::new(opener).arg(dir).spawn() {
            log::warn!("could not reveal {}: {err}", path.display());
        }
    }

    pub fn install(_notify: crate::player::Notify) {}

    pub fn drain() -> Vec<PathBuf> {
        Vec::new()
    }
}

pub use imp::{paste_text, choose_files, copy_png, drain, install, reveal, sync_menus, take_menu_requests, take_view_request};
