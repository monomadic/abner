//! abner — A/B video comparison player.
//!
//! Plays two or more videos in frame-locked sync and flips/blends/diffs
//! between them. The window loop carries switchblade's learnings: idle
//! throttling with a min-frame floor, the occlusion guard (an occluded
//! surface never presents, so a Poll loop would peg a core), worker wakes
//! coalesced into single redraws, and the fake-fullscreen +
//! window-shadow trick for macOS Tahoe's contour line.

mod app;
mod player;
mod probe;
mod render;
mod schedule;
mod text;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use winit::application::ApplicationHandler;
use winit::dpi::LogicalSize;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key as WinitKey, NamedKey};
use winit::window::{Window, WindowId};

use app::{App, Cmd, Key, Video};
use player::Player;
use render::Gpu;
use text::TextCtx;

const USAGE: &str = "\
abner — A/B video comparison player

usage: abner [--view <overlay|sbs|delta|split|checker|blend>] [<video-a> <video-b> [more...]]

Run with no arguments (or launched from the .app bundle) to open the
launch window, then drag clips onto it: one drop fills slot A and waits,
two fill A and B and start playing, more add C, D… Dropping onto a
running comparison ADDS streams; hold Cmd while dropping to replace the
whole set. A single path on the command line loads slot A the same way.

keys:
  Enter        flip to the next video (overlay mode)
  Space        pause / play
  < >  (, .)   frame-step back / forward
  ← →          seek ±1s
  [ ]          slow down / speed up playback (Backspace resets)
  1..6         view: 1 overlay  2 side-by-side  3 delta  4 split  5 checker  6 blend
  - =          adjust delta gain / blend / checker size
  pinch        zoom on the pointer, photo-style (drag or scroll to pan; synced)
  Z            reset zoom
  F            fullscreen (borderless, same Space)
  Tab          toggle info overlay
  Q            quit
  Esc          leave fullscreen, else quit
";

fn main() -> anyhow::Result<()> {
    env_logger::init();
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        print!("{USAGE}");
        return Ok(());
    }
    let mut mode = app::Mode::Overlay;
    let mut paths: Vec<PathBuf> = Vec::new();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        if a == "--view" {
            let v = it.next().and_then(|v| app::Mode::parse(v));
            match v {
                Some(m) => mode = m,
                None => {
                    eprintln!("--view needs one of: overlay sbs delta split checker blend");
                    std::process::exit(2);
                }
            }
        } else if !a.starts_with('-') {
            paths.push(PathBuf::from(a));
        }
    }
    // Zero paths (bundle double-click / bare `abner`) opens the launch
    // window; one path fills slot A there and waits for a drop — the same
    // half-filled state a single dropped file produces.
    let mut videos = Vec::new();
    for p in &paths {
        videos.push(load_video(p)?);
    }

    let event_loop = EventLoop::new()?;
    // NSApp exists once the event loop is built; give it our icon before
    // the window (and therefore the Dock tile) appears.
    set_app_icon();
    event_loop.set_control_flow(ControlFlow::Poll);
    // Frame-arrival wakes (paused framesteps, seek completions) coalesce
    // into single redraws via the event-loop proxy.
    let proxy = event_loop.create_proxy();
    let notify: player::Notify = Arc::new(move || {
        let _ = proxy.send_event(());
    });
    for v in &videos {
        v.player.set_notify(notify.clone());
    }

    let title = title_for(&videos);

    let mut runner = Runner {
        app: App::new(videos, mode),
        title,
        notify,
        dropped: Vec::new(),
        window: None,
        gpu: None,
        last_frame: Instant::now(),
        cursor: (0.0, 0.0),
        animating: true,
        redraw_at: None,
        occluded: false,
    };
    event_loop.run_app(&mut runner)?;
    Ok(())
}

/// Probe one path and start its decoder. The probe is a synchronous
/// ffprobe under a hard deadline (`probe::run_deadlined`), which is what
/// makes it safe to call straight from the event loop on a drop: a file
/// on a dead mount fails in bounded time instead of hanging the window.
fn load_video(path: &Path) -> anyhow::Result<Video> {
    let info = probe::probe(path)?;
    log::info!(
        "{}: {}x{} {} {:.3}fps",
        path.display(),
        info.width,
        info.height,
        info.codec,
        info.fps
    );
    let player = Player::spawn(
        path,
        info.width,
        info.height,
        probe::vt_accel(&info.codec),
        info.rotation,
    )
    .ok_or_else(|| anyhow::anyhow!("failed to start decoder for {}", path.display()))?;
    Ok(Video { info, player, shown_pts: 0.0, delivered: false, pending: false })
}

fn title_for(videos: &[Video]) -> String {
    if videos.is_empty() {
        return "abner".to_string();
    }
    let names: Vec<String> = videos
        .iter()
        .map(|v| {
            v.info.path.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default()
        })
        .collect();
    format!("abner — {}", names.join(" vs "))
}

struct Runner {
    app: App,
    title: String,
    /// Handed to every player, including ones spawned by a drop, so a
    /// frame arriving while the loop idles still wakes it.
    notify: player::Notify,
    /// Files dropped this loop turn. winit reports one `DroppedFile` per
    /// file with NO end-of-batch event, so they collect here and flush as
    /// one gesture in `about_to_wait` — switchblade's `FilesDropped`
    /// path. Without the batch, dropping two clips at once would land as
    /// two separate one-file loads.
    dropped: Vec<PathBuf>,
    window: Option<Arc<Window>>,
    gpu: Option<Gpu>,
    last_frame: Instant,
    cursor: (f32, f32),
    animating: bool,
    redraw_at: Option<Instant>,
    occluded: bool,
}

impl Runner {
    fn scale(&self) -> f64 {
        self.window.as_ref().map_or(1.0, |w| w.scale_factor())
    }

    /// One drop gesture, flushed whole from `about_to_wait`.
    ///
    /// Each path is probed and spawned right here — a bad file (a folder,
    /// a PDF, a clip on a vanished mount) is logged and skipped rather
    /// than taken as a reason to fail the app, because a drop is a guess
    /// by definition. Survivors fill the next free slots; `replace` (⌘
    /// held) starts over from the first of them.
    fn files_dropped(&mut self, paths: Vec<PathBuf>, replace: bool) {
        self.app.set_drag_hover(false);
        let mut videos = Vec::new();
        for p in &paths {
            match load_video(p) {
                Ok(v) => {
                    v.player.set_notify(self.notify.clone());
                    videos.push(v);
                }
                Err(e) => log::warn!("ignoring dropped {}: {e}", p.display()),
            }
        }
        if videos.is_empty() {
            return;
        }
        log::info!(
            "{} clip(s) dropped on the window ({})",
            videos.len(),
            if replace { "replace" } else { "add" }
        );
        self.app.add_videos(videos, replace);
        // The GPU's per-video textures are indexed by video slot, so they
        // follow the app's list; unchanged slots keep their texture (see
        // `set_video_dims`), so an append never blanks what is on screen.
        let dims: Vec<(u32, u32)> =
            self.app.videos.iter().map(|v| (v.player.w, v.player.h)).collect();
        if let Some(gpu) = &mut self.gpu {
            gpu.set_video_dims(&dims);
        }
        self.title = title_for(&self.app.videos);
        if let Some(w) = &self.window {
            w.set_title(&self.title);
        }
    }

    fn apply_cmds(&mut self, event_loop: &ActiveEventLoop) {
        for cmd in self.app.take_cmds() {
            match cmd {
                Cmd::Quit => event_loop.exit(),
                Cmd::ToggleFullscreen => {
                    if let Some(w) = &self.window {
                        toggle_fast_fullscreen(w);
                    }
                }
            }
        }
    }
}

/// mpv-style "fake" fullscreen: a borderless desktop-sized window on the
/// same Space (instant, no macOS fullscreen animation). macOS Tahoe (26)
/// draws a ~1px translucent contour around every non-native-fullscreen
/// window; AppKit ties that edge to the window shadow, so
/// `setHasShadow(false)` suppresses it — a desktop-filling window has no
/// visible shadow to lose. (Straight from switchblade.)
fn toggle_fast_fullscreen(w: &Window) {
    #[cfg(target_os = "macos")]
    {
        use winit::platform::macos::WindowExtMacOS;
        let entering = !w.simple_fullscreen();
        w.set_simple_fullscreen(entering);
        set_window_shadow(w, !entering);
    }
    #[cfg(not(target_os = "macos"))]
    {
        use winit::window::Fullscreen;
        let next = if w.fullscreen().is_some() { None } else { Some(Fullscreen::Borderless(None)) };
        w.set_fullscreen(next);
    }
}

/// The app icon, baked into the executable.
///
/// A bare Mach-O has nowhere to hang an icon — `CFBundleIconFile` only
/// resolves inside an .app — so the PNG rides along in `.rodata` and we hand
/// it to AppKit at startup. That covers the common case here (`abner a.mp4
/// b.mp4` from a shell), where there is no bundle at all.
///
/// `assets/app-icon.png` is the icon SLOT: `packaging/build-app.sh` renders
/// the bundle's AppIcon.icns from the same file, so the two can't drift.
#[cfg(target_os = "macos")]
const ICON_PNG: &[u8] = include_bytes!("../assets/app-icon.png");

/// Sets the Dock / app-switcher icon for the BARE binary. AppKit decodes the
/// PNG itself, so no image crate is pulled in for this. (No-op elsewhere:
/// X11/Windows want an already-decoded RGBA buffer via
/// `Window::with_window_icon`, which would mean a PNG decoder — not worth it
/// until abner runs there.)
///
/// **Inside a bundle this does nothing, and that is the whole subtlety**
/// (switchblade, 2026-08-30): `setApplicationIconImage` OVERRIDES
/// `AppIcon.icns` the moment the app launches, so a bundle whose Resources
/// hold the current icon still showed the baked-in image in the Dock and
/// cmd-tab. It read exactly like a stale icon cache — rebuilding,
/// reinstalling, re-registering with LaunchServices and killing the Dock all
/// changed nothing, because the running app was repainting its own tile
/// every launch. A bundle already declares its icon in Info.plist; the
/// runtime call is for the case that can't.
#[cfg(target_os = "macos")]
fn set_app_icon() {
    use objc2::ClassType;
    use objc2_app_kit::{NSApplication, NSImage};
    use objc2_foundation::{MainThreadMarker, NSData};
    let Some(mtm) = MainThreadMarker::new() else { return };
    // Bundled? Then Info.plist's CFBundleIconFile is the authority. The
    // executable sits at Abner.app/Contents/MacOS/<bin>, which is the
    // cheapest honest test and needs no NSBundle feature.
    let bundled = std::env::current_exe()
        .ok()
        .and_then(|p| Some(p.parent()?.parent()?.file_name()? == "Contents"))
        .unwrap_or(false);
    if bundled {
        return;
    }
    let data = NSData::with_bytes(ICON_PNG);
    let Some(image) = NSImage::initWithData(NSImage::alloc(), &data) else {
        log::warn!("embedded app icon failed to decode");
        return;
    };
    unsafe { NSApplication::sharedApplication(mtm).setApplicationIconImage(Some(&image)) };
}

#[cfg(not(target_os = "macos"))]
fn set_app_icon() {}

/// Glass titlebar: hide the title text, drop the titlebar's own drawing,
/// and — crucially — extend the content view UNDER it
/// (`FullSizeContentView`) so the wgpu surface fills that strip too. A
/// transparent titlebar on its own shows the DEFAULT system window tint
/// (a grey), not the app's clear, so letting the same GPU clear paint it
/// is the only way the strip matches the frame exactly. The traffic-light
/// buttons float above the content and are untouched; the window title is
/// still set (and still correct for anything that reads it), just hidden.
/// The app's top HUD row keeps clear of the buttons via
/// `App::top_inset`. (switchblade, `set_titlebar_glass`.)
#[cfg(target_os = "macos")]
fn set_titlebar_glass(w: &Window) {
    use objc2_app_kit::{NSView, NSWindowStyleMask, NSWindowTitleVisibility};
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = w.window_handle() else { return };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else { return };
    let view: &NSView = unsafe { h.ns_view.cast::<NSView>().as_ref() };
    if let Some(window) = view.window() {
        window.setTitlebarAppearsTransparent(true);
        window.setTitleVisibility(NSWindowTitleVisibility::NSWindowTitleHidden);
        // Additive, so winit's existing mask bits survive — and winit
        // saves/restores the whole mask around simple fullscreen, so this
        // bit comes back with it.
        window.setStyleMask(window.styleMask() | NSWindowStyleMask::FullSizeContentView);
    }
}

/// Whether the platform's primary modifier (⌘ on macOS) is held RIGHT
/// NOW, read from the hardware state. Only needed where the event stream
/// can't answer: a drag from another app delivers `DroppedFile` without
/// this window ever seeing a `ModifiersChanged`. (switchblade.)
fn os_primary_modifier_down() -> bool {
    #[cfg(target_os = "macos")]
    {
        use objc2_app_kit::{NSEvent, NSEventModifierFlags};
        unsafe { NSEvent::modifierFlags_class() }
            .contains(NSEventModifierFlags::NSEventModifierFlagCommand)
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

#[cfg(target_os = "macos")]
fn set_window_shadow(w: &Window, on: bool) {
    use objc2_app_kit::NSView;
    use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
    let Ok(handle) = w.window_handle() else { return };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else { return };
    let view: &NSView = unsafe { h.ns_view.cast::<NSView>().as_ref() };
    if let Some(window) = view.window() {
        window.setHasShadow(on);
    }
}

impl ApplicationHandler for Runner {
    fn resumed(&mut self, _event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attrs = Window::default_attributes()
            .with_title(&self.title)
            // The app background is drawn with an alpha (see FRAME_BG /
            // LAUNCH_BG in app.rs), so the desktop shows faintly through
            // the letterbox and the launch window. Video quads write
            // alpha 1, so the picture itself is never see-through.
            .with_transparent(true)
            .with_inner_size(LogicalSize::new(1280.0, 800.0));
        let window = Arc::new(_event_loop.create_window(attrs).expect("create window"));
        // Text-free glass titlebar with the video running underneath it
        // (traffic lights kept) — switchblade's treatment.
        #[cfg(target_os = "macos")]
        set_titlebar_glass(&window);
        let dims: Vec<(u32, u32)> =
            self.app.videos.iter().map(|v| (v.player.w, v.player.h)).collect();
        let gpu = pollster::block_on(Gpu::new(window.clone(), &dims, TextCtx::load()))
            .expect("init gpu");
        // The renderer decoded the wordmark, so it owns its proportions;
        // the launch window sizes its quad from them.
        self.app.set_logo_aspect(gpu.logo_aspect());
        self.window = Some(window);
        self.gpu = Some(gpu);
        self.last_frame = Instant::now();
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Any interaction wakes the loop optimistically; the next frame's
        // `animating` verdict decides whether it stays awake.
        if matches!(
            event,
            WindowEvent::KeyboardInput { .. }
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::PinchGesture { .. }
                | WindowEvent::CursorMoved { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::Resized(_)
                | WindowEvent::Focused(_)
        ) {
            self.animating = true;
        }
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(gpu) = &mut self.gpu {
                    gpu.resize(size.width, size.height);
                }
            }
            WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
                if let Some(gpu) = &mut self.gpu {
                    gpu.set_scale(scale_factor as f32);
                }
            }
            WindowEvent::DroppedFile(path) => {
                // One event per file, no end-of-batch marker: collect and
                // flush the whole gesture in `about_to_wait`.
                self.dropped.push(path);
                self.animating = true;
            }
            WindowEvent::HoveredFile(_) => {
                self.app.set_drag_hover(true);
                self.animating = true;
            }
            WindowEvent::HoveredFileCancelled => {
                self.app.set_drag_hover(false);
                self.animating = true;
            }
            WindowEvent::Occluded(occluded) => {
                self.occluded = occluded;
                if !occluded {
                    self.animating = true;
                }
            }
            WindowEvent::KeyboardInput { event, .. } => {
                if event.state != ElementState::Pressed {
                    return;
                }
                let key = match &event.logical_key {
                    WinitKey::Named(NamedKey::ArrowLeft) => Some(Key::Left),
                    WinitKey::Named(NamedKey::ArrowRight) => Some(Key::Right),
                    WinitKey::Named(NamedKey::Enter) => Some(Key::Enter),
                    WinitKey::Named(NamedKey::Space) => Some(Key::Space),
                    WinitKey::Named(NamedKey::Escape) => Some(Key::Escape),
                    WinitKey::Named(NamedKey::Tab) => Some(Key::Tab),
                    WinitKey::Named(NamedKey::Backspace) => Some(Key::Backspace),
                    WinitKey::Character(s) => s.chars().next().map(Key::Char),
                    _ => None,
                };
                if let Some(key) = key {
                    self.app.key(key);
                    self.apply_cmds(event_loop);
                }
            }
            WindowEvent::PinchGesture { delta, .. } => {
                self.app.pinch(delta as f32);
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let (dx, dy) = match delta {
                    MouseScrollDelta::PixelDelta(p) => {
                        let s = self.scale() as f32;
                        (p.x as f32 / s, p.y as f32 / s)
                    }
                    MouseScrollDelta::LineDelta(x, y) => (x * 40.0, y * 40.0),
                };
                self.app.scroll(dx, dy);
            }
            WindowEvent::CursorMoved { position, .. } => {
                let p = position.to_logical::<f32>(self.scale());
                self.cursor = (p.x, p.y);
                self.app.cursor_moved(p.x, p.y);
            }
            WindowEvent::MouseInput { state, button: MouseButton::Left, .. } => {
                let (x, y) = self.cursor;
                match state {
                    ElementState::Pressed => self.app.mouse_down(x, y),
                    ElementState::Released => self.app.mouse_up(),
                }
            }
            WindowEvent::RedrawRequested => {
                let now = Instant::now();
                let dt = (now - self.last_frame).as_secs_f32().min(0.05);
                self.last_frame = now;
                let (Some(window), Some(gpu)) = (&self.window, &mut self.gpu) else {
                    return;
                };
                let scale = window.scale_factor() as f32;
                let size = window.inner_size();
                let vp = (size.width as f32 / scale, size.height as f32 / scale);
                let desc = self.app.tick(dt, vp, scale);
                gpu.render(&desc, vp);
                self.animating = desc.animating;
                self.redraw_at = desc.redraw_at;
                // Frame buffers go back to their decoders' pools.
                for u in desc.uploads {
                    self.app.recycle(u.idx, u.buf);
                }
                self.apply_cmds(event_loop);
            }
            _ => {}
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, _event: ()) {
        // A decoder delivered a frame while the loop idled (paused
        // framestep, seek completion): one redraw services it. Skipped
        // while occluded — the idle tick keeps housekeeping alive.
        if self.occluded {
            return;
        }
        if let Some(w) = &self.window {
            w.request_redraw();
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        // A drop gesture's per-file events have all been dispatched by the
        // time the loop is about to wait: flush them as one batch, so two
        // files dropped together land as an A/B pair. Slot order is the
        // order the platform delivers (for a Finder multi-select, the
        // order they appear in that window) — drop them one at a time to
        // choose. ⌘ (replace instead
        // of add) is read from the HARDWARE modifier state — a drag from
        // Finder never focuses this window, so no `ModifiersChanged` ever
        // reported the key going down. (switchblade, `FilesDropped`.)
        if !self.dropped.is_empty() {
            let paths = std::mem::take(&mut self.dropped);
            self.files_dropped(paths, os_primary_modifier_down());
            self.animating = true;
            if let Some(w) = &self.window {
                w.request_redraw();
            }
        }
        // The cadence rules live in `schedule` (tested there), not here.
        let Some(w) = &self.window else { return };
        match schedule::next_frame(
            self.animating,
            self.occluded,
            self.redraw_at,
            self.last_frame,
            Instant::now(),
        ) {
            schedule::NextFrame::Now { poll } => {
                if poll {
                    event_loop.set_control_flow(ControlFlow::Poll);
                }
                w.request_redraw();
            }
            schedule::NextFrame::At(next) => {
                event_loop.set_control_flow(ControlFlow::WaitUntil(next));
            }
        }
    }
}
