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
mod text;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
launch window and drop clips in.

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
  Q / Esc      quit
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
    // window; one path can't form an A/B pair, so that's still an error.
    if paths.len() == 1 {
        eprintln!("abner compares two or more videos — pass none to open the launch window, or at least two.\n");
        eprint!("{USAGE}");
        std::process::exit(2);
    }

    let mut videos = Vec::new();
    for p in &paths {
        let info = probe::probe(p)?;
        log::info!(
            "{}: {}x{} {} {:.3}fps",
            p.display(),
            info.width,
            info.height,
            info.codec,
            info.fps
        );
        let player = Player::spawn(
            p,
            info.width,
            info.height,
            probe::vt_accel(&info.codec),
            info.rotation,
        )
        .ok_or_else(|| anyhow::anyhow!("failed to start decoder for {}", p.display()))?;
        videos.push(Video { info, player, shown_pts: 0.0, delivered: false, pending: false });
    }

    let event_loop = EventLoop::new()?;
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

    let title = if paths.is_empty() {
        "abner".to_string()
    } else {
        let names: Vec<String> = paths
            .iter()
            .map(|p| p.file_name().map(|f| f.to_string_lossy().into_owned()).unwrap_or_default())
            .collect();
        format!("abner — {}", names.join(" vs "))
    };

    let mut runner = Runner {
        app: App::new(videos, mode),
        title,
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

/// Floor on the continuous-redraw interval — the runaway guard for any
/// path that skips the vsync present (switchblade's occluded-window bug).
const MIN_FRAME: Duration = Duration::from_millis(4);
/// Housekeeping cadence while nothing animates.
const IDLE_TICK: Duration = Duration::from_millis(100);

struct Runner {
    app: App,
    title: String,
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
            .with_inner_size(LogicalSize::new(1280.0, 800.0));
        let window = Arc::new(_event_loop.create_window(attrs).expect("create window"));
        let dims: Vec<(u32, u32)> =
            self.app.videos.iter().map(|v| (v.player.w, v.player.h)).collect();
        let gpu = pollster::block_on(Gpu::new(window.clone(), &dims, TextCtx::load()))
            .expect("init gpu");
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
        let Some(w) = &self.window else { return };
        let now = Instant::now();
        if self.animating && !self.occluded {
            let next = self.last_frame + MIN_FRAME;
            if now >= next {
                event_loop.set_control_flow(ControlFlow::Poll);
                w.request_redraw();
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(next));
            }
        } else {
            let mut next = self.last_frame + IDLE_TICK;
            if !self.occluded && let Some(t) = self.redraw_at {
                next = next.min(t);
            }
            if now >= next {
                w.request_redraw();
            } else {
                event_loop.set_control_flow(ControlFlow::WaitUntil(next));
            }
        }
    }
}
