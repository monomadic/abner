//! App state and per-frame logic: the master clock, view modes, input,
//! and the UI overlay.
//!
//! Sync model: one master time `t` advances by wall-clock dt while
//! playing; every player queues `(pts, rgba)` frames and each frame the
//! app pops everything `pts <= t` (newest wins). All streams answer to
//! the same clock, so switching the displayed video (Enter) can never
//! jump in time — the other stream was already decoding the same moment.

use std::time::Instant;

use crate::player::Player;
use crate::mask::{self, Mask};
use crate::probe::VideoInfo;
use crate::render::{
    Align, FrameDesc, Item, RectItem, RectPx, TextItem, Upload, VAlign, VideoMode,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Videos stacked on top of each other; Enter flips which one shows.
    Overlay,
    SideBySide,
    /// Amplified |A−B| difference.
    Delta,
    /// Vertical wipe, divider follows the pointer.
    Split,
    Checker,
    /// 50/50 (adjustable) mix.
    Blend,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Overlay => "overlay",
            Mode::SideBySide => "side-by-side",
            Mode::Delta => "delta",
            Mode::Split => "split",
            Mode::Checker => "checker",
            Mode::Blend => "blend",
        }
    }
    const ALL: [Mode; 6] =
        [Mode::Overlay, Mode::SideBySide, Mode::Delta, Mode::Split, Mode::Checker, Mode::Blend];
    /// Next (or previous) view in `V`'s cycle.
    fn cycle(self, dir: i32) -> Mode {
        let i = Self::ALL.iter().position(|m| *m == self).unwrap() as i32;
        Self::ALL[(i + dir).rem_euclid(Self::ALL.len() as i32) as usize]
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Left,
    Right,
    Enter,
    Space,
    Escape,
    Tab,
    Backspace,
}

#[derive(Debug, Clone, Copy)]
pub enum Cmd {
    Quit,
    ToggleFullscreen,
}

pub struct Video {
    pub info: VideoInfo,
    pub player: Player,
    pub shown_pts: f64,
    pub delivered: bool,
    /// Waiting to adopt the first frame after an exact seek while paused.
    pub pending: bool,
}

pub struct App {
    pub videos: Vec<Video>,
    active: usize,
    masks: Vec<Option<Mask>>,
    mask_mode: bool,
    brush_diameter: f32,
    painting: bool,
    stroke_last: Option<(f32, f32)>,
    cursor_inside: bool,
    mask_status: String,
    mask_save: Option<std::sync::mpsc::Receiver<Result<std::path::PathBuf, String>>>,
    /// Master clock, seconds of content time.
    t: f64,
    playing: bool,
    /// Clock held at 0 until every stream delivered its first frame, so
    /// all streams anchor on the same instant.
    started: bool,
    mode: Mode,
    /// Photo-style zoom: 1.0 = fit; pinch scales around the pointer.
    zoom: f32,
    /// Content point (0..1 of the video) held at the view's center. All
    /// videos share it, so pan/zoom stays position-synced across streams.
    center: (f32, f32),
    /// Playback rate multiplier (`[`/`]`, Backspace resets).
    speed: f64,
    show_ui: bool,
    /// Delta amplification.
    gain: f32,
    blend: f32,
    checker_px: f32,
    /// Seconds left on the big active-letter flash (shown after Enter
    /// when the UI is hidden — switchblade's skip-bar-flash pattern).
    badge_flash: f32,
    fullscreen: bool,
    cursor: (f32, f32),
    /// Last pointer position while a drag-pan is held.
    drag: Option<(f32, f32)>,
    /// Seconds of pointer stillness — drives the transport's reveal.
    since_pointer: f32,
    /// Dragging the seek bar (pins the transport open).
    scrubbing: bool,
    vp: (f32, f32),
    fps: f64,
    /// Loop point: shortest stream duration (∞ when unknown).
    wrap: f64,
    /// A file drag is hovering the window — brightens the launch
    /// window's drop targets. winit reports no drop POSITION, so the
    /// whole window is one target and both zones light together.
    drag_hover: bool,
    /// Width / height of the wordmark texture, handed over by the
    /// renderer once it has decoded the image (`Gpu::logo_aspect`). The
    /// placeholder only ever shows in tests, which draw no pixels.
    logo_aspect: f32,
    cmds: Vec<Cmd>,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Mode> {
        Some(match s {
            "1" | "overlay" => Mode::Overlay,
            "2" | "sbs" | "side-by-side" => Mode::SideBySide,
            "3" | "delta" => Mode::Delta,
            "4" | "split" => Mode::Split,
            "5" | "checker" => Mode::Checker,
            "6" | "blend" => Mode::Blend,
            _ => return None,
        })
    }
}

impl App {
    pub fn new(videos: Vec<Video>, mode: Mode) -> Self {
        let fps = Self::fps_of(&videos);
        let wrap = Self::wrap_of(&videos);
        Self {
            masks: (0..videos.len()).map(|_| None).collect(),
            mask_mode: false,
            brush_diameter: 64.0,
            painting: false,
            stroke_last: None,
            cursor_inside: false,
            mask_status: String::new(),
            mask_save: None,
            videos,
            active: 0,
            t: 0.0,
            playing: true,
            started: false,
            mode,
            zoom: 1.0,
            center: (0.5, 0.5),
            speed: 1.0,
            show_ui: true,
            gain: 4.0,
            blend: 0.5,
            checker_px: 48.0,
            badge_flash: 0.0,
            fullscreen: false,
            cursor: (0.0, 0.0),
            drag: None,
            since_pointer: 0.0,
            scrubbing: false,
            vp: (1280.0, 800.0),
            fps,
            wrap,
            drag_hover: false,
            logo_aspect: 3.0,
            cmds: Vec::new(),
        }
    }

    fn ensure_mask(&mut self) {
        if self.masks[self.active].is_none() {
            let player = &self.videos[self.active].player;
            self.masks[self.active] = Some(Mask::new(player.w, player.h));
        }
    }

    fn resize_brush(&mut self, factor: f32) {
        self.brush_diameter = (self.brush_diameter * factor).clamp(1.0, 4096.0);
        self.stroke_last = None;
    }

    pub fn cursor_left(&mut self) {
        self.cursor_inside = false;
        self.mouse_up();
    }

    /// The mask uses exactly the full-window video transform. The status line,
    /// the traffic-light strip and the letterbox are not paint targets; leaving
    /// them breaks stroke continuity.
    pub fn brush_cursor_visible(&self) -> bool {
        if !self.mask_mode || !self.cursor_inside || !self.ready() { return false; }
        let r = self.content_rect(self.active);
        let (x, y) = self.cursor;
        x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
            && x >= 0.0 && x < self.vp.0 && y >= self.top_inset() && y < self.vp.1 - STATUS_H
    }

    fn paint_at_cursor(&mut self) {
        if !self.brush_cursor_visible() { self.stroke_last = None; return; }
        let r = self.content_rect(self.active);
        let mask = self.masks[self.active].as_mut().unwrap();
        let point = ((self.cursor.0 - r.x) / r.w * mask.width as f32,
                     (self.cursor.1 - r.y) / r.h * mask.height as f32);
        mask.paint(self.stroke_last.unwrap_or(point), point, self.brush_diameter * 0.5);
        self.stroke_last = Some(point);
        self.mask_status.clear();
    }

    fn save_mask(&mut self) {
        if self.mask_save.is_some() { return; }
        let mask = self.masks[self.active].as_ref().unwrap();
        let (width, height, pixels) = (mask.width, mask.height, mask.pixels.clone());
        let path = mask::output_path(&self.videos[self.active].info.path);
        let (sender, receiver) = std::sync::mpsc::channel();
        self.mask_save = Some(receiver);
        self.mask_status = "Saving…".into();
        std::thread::spawn(move || {
            let result = mask::save(&path, width, height, &pixels)
                .map(|()| path).map_err(|e| e.to_string());
            let _ = sender.send(result);
        });
    }

    fn build_mask_layer(&self, items: &mut Vec<Item>) {
        let mask = self.masks[self.active].as_ref().unwrap();
        let r = self.content_rect(self.active);
        items.push(Item::Mask { r, id: mask.id, revision: mask.revision,
            width: mask.width, height: mask.height, pixels: mask.pixels.clone() });
        if self.brush_cursor_visible() {
            let diameter = self.brush_diameter * r.w / mask.width as f32;
            // Two contrasting outlines keep the actual brush footprint visible
            // over both overlay colours and bright or dark footage.
            for (extra, width, color) in [(2.0, 3.0, [0.0, 0.0, 0.0, 0.9]), (0.0, 1.0, [1.0; 4])] {
                let d = diameter + extra;
                items.push(Item::Rect(RectItem {
                    r: RectPx { x: self.cursor.0 - d * 0.5, y: self.cursor.1 - d * 0.5, w: d, h: d },
                    radius: d * 0.5, border_w: width, border_color: color,
                    color: [0.0; 4], ..Default::default()
                }));
            }
        }
    }

    pub fn set_logo_aspect(&mut self, aspect: f32) {
        self.logo_aspect = aspect;
    }

    /// Top margin for HUD rows that would otherwise sit under the
    /// floating traffic lights — see `TITLEBAR_H`.
    fn top_inset(&self) -> f32 {
        if self.fullscreen { 0.0 } else { TITLEBAR_H }
    }

    /// Something to show. One clip (a single drop, or `abner one.mp4`)
    /// is enough: it lands in slot A and plays; the compare modes just
    /// have nothing to compare against until a second one arrives.
    pub fn ready(&self) -> bool {
        !self.videos.is_empty()
    }

    /// The mode actually drawn. A lone clip has nothing to compare
    /// against (b == a: the delta would be a black frame), so every mode
    /// shows it plain; `self.mode` is kept for when a second one arrives.
    fn shown_mode(&self) -> Mode {
        if self.videos.len() < 2 { Mode::Overlay } else { self.mode }
    }

    fn fps_of(videos: &[Video]) -> f64 {
        videos.first().map(|v| v.info.fps).filter(|f| *f > 1.0).unwrap_or(30.0)
    }

    fn wrap_of(videos: &[Video]) -> f64 {
        videos
            .iter()
            .map(|v| v.info.duration)
            .filter(|d| *d > 0.1)
            .fold(f64::INFINITY, f64::min)
    }

    /// Take on newly loaded clips — from a drop, or (later) an Open With.
    /// They fill the next free slots in drop order (A, B, C…); `replace`
    /// starts a fresh comparison from the first of them instead.
    ///
    /// Every stream rewinds to 0: the arrivals decode from the top, and a
    /// clip already running at t=42 that kept its position would be
    /// unsynced against them — the one thing this product must never
    /// show. Frame-lock is re-established by construction, as at startup.
    pub fn add_videos(&mut self, mut videos: Vec<Video>, replace: bool) {
        if videos.is_empty() {
            return;
        }
        if replace {
            self.videos.clear();
            self.masks.clear();
        }
        for v in &mut self.videos {
            v.player.seek(0.0, true);
            v.pending = false;
            v.delivered = false;
        }
        self.masks.extend((0..videos.len()).map(|_| None));
        self.mask_mode = false;
        self.painting = false;
        self.stroke_last = None;
        self.mask_status.clear();
        self.videos.append(&mut videos);
        self.fps = Self::fps_of(&self.videos);
        self.wrap = Self::wrap_of(&self.videos);
        self.t = 0.0;
        self.started = false;
        self.playing = true;
        self.active = 0;
        self.speed = 1.0;
        self.zoom = 1.0;
        self.center = (0.5, 0.5);
        self.since_pointer = 0.0;
        self.drag = None;
        self.scrubbing = false;
        self.drag_hover = false;
    }

    /// A file drag entered or left the window (winit's `HoveredFile` /
    /// `HoveredFileCancelled`).
    pub fn set_drag_hover(&mut self, on: bool) {
        self.drag_hover = on;
    }

    pub fn take_cmds(&mut self) -> Vec<Cmd> {
        std::mem::take(&mut self.cmds)
    }

    pub fn recycle(&mut self, idx: usize, buf: Vec<u8>) {
        if let Some(v) = self.videos.get(idx) {
            v.player.recycle(buf);
        }
    }

    fn seek_all(&mut self, target: f64, exact: bool) {
        for v in &mut self.videos {
            v.player.seek(target, exact);
        }
        self.t = target;
        if !self.playing {
            for v in &mut self.videos {
                v.pending = true;
            }
        }
    }

    /// Step one frame. Targets sit half a frame period past/before the
    /// current frame so pts rounding can't land on the same frame; the
    /// delivered frame's true pts is then adopted as the clock.
    fn step(&mut self, dir: i32) {
        let d = 1.0 / self.fps;
        self.playing = false;
        let target = if dir > 0 { self.t + 0.5 * d } else { (self.t - 1.5 * d).max(0.0) };
        self.seek_all(target, true);
    }

    fn seek_by(&mut self, delta: f64) {
        let max = if self.wrap.is_finite() { self.wrap - 0.05 } else { f64::MAX };
        let target = (self.t + delta).clamp(0.0, max.max(0.0));
        self.seek_all(target, true);
    }

    fn adjust_param(&mut self, up: bool) {
        match self.mode {
            Mode::Blend => {
                self.blend = (self.blend + if up { 0.1 } else { -0.1 }).clamp(0.0, 1.0);
            }
            Mode::Checker => {
                self.checker_px =
                    (self.checker_px * if up { 1.5 } else { 1.0 / 1.5 }).clamp(4.0, 512.0);
            }
            _ => {
                self.gain = (self.gain * if up { 1.5 } else { 1.0 / 1.5 }).clamp(1.0, 64.0);
            }
        }
    }

    pub fn key(&mut self, k: Key) {
        // Launch state (no clips): only global keys are live.
        if !self.ready() {
            match k {
                Key::Escape => {
                    if self.fullscreen {
                        self.fullscreen = false;
                        self.cmds.push(Cmd::ToggleFullscreen);
                    } else {
                        self.cmds.push(Cmd::Quit);
                    }
                }
                Key::Char(c) => match c.to_ascii_lowercase() {
                    'q' => self.cmds.push(Cmd::Quit),
                    'f' => {
                        self.fullscreen = !self.fullscreen;
                        self.cmds.push(Cmd::ToggleFullscreen);
                    }
                    _ => {}
                },
                _ => {}
            }
            return;
        }
        if k == Key::Char('m') || k == Key::Char('M') {
            self.mask_mode = !self.mask_mode;
            self.mouse_up();
            if self.mask_mode {
                self.playing = false;
                self.ensure_mask();
            }
            return;
        }
        if self.mask_mode {
            match k {
                Key::Char('+' | '=') => { self.resize_brush(1.25); return; }
                Key::Char('-' | '_') => { self.resize_brush(0.8); return; }
                Key::Char('s' | 'S') => { self.save_mask(); return; }
                Key::Escape => { self.mask_mode = false; self.mouse_up(); return; }
                _ => {}
            }
            self.stroke_last = None;
        }
        match k {
            Key::Enter => self.select((self.active + 1) % self.videos.len()),
            Key::Space => self.playing = !self.playing,
            Key::Tab => self.show_ui = !self.show_ui,
            Key::Left => self.seek_by(-1.0),
            Key::Right => self.seek_by(1.0),
            Key::Escape => {
                if self.fullscreen {
                    self.fullscreen = false;
                    self.cmds.push(Cmd::ToggleFullscreen);
                } else {
                    self.cmds.push(Cmd::Quit);
                }
            }
            Key::Backspace => self.speed = 1.0,
            Key::Char('V') => self.mode = self.mode.cycle(-1),
            Key::Char(c @ '1'..='9') => {
                let idx = c as usize - '1' as usize;
                if idx < self.videos.len() {
                    self.select(idx);
                }
            }
            Key::Char(c) => match c.to_ascii_lowercase() {
                'q' => self.cmds.push(Cmd::Quit),
                'f' => {
                    self.fullscreen = !self.fullscreen;
                    self.cmds.push(Cmd::ToggleFullscreen);
                }
                'z' => {
                    self.zoom = 1.0;
                    self.center = (0.5, 0.5);
                }
                '[' => self.speed = (self.speed / 1.25).max(0.25),
                ']' => self.speed = (self.speed * 1.25).min(4.0),
                ',' | '<' => self.step(-1),
                '.' | '>' => self.step(1),
                '-' => self.adjust_param(false),
                '=' | '+' => self.adjust_param(true),
                'v' => self.mode = self.mode.cycle(1),
                _ => {}
            },
        }
    }

    /// Show clip `idx` (Enter's flip, or its number key directly).
    fn select(&mut self, idx: usize) {
        self.active = idx;
        self.badge_flash = 1.2;
        self.mouse_up();
        if self.mask_mode { self.ensure_mask(); self.mask_status.clear(); }
    }

    pub fn cursor_moved(&mut self, x: f32, y: f32) {
        self.cursor_inside = true;
        if self.mask_mode {
            self.cursor = (x, y);
            if self.painting { self.paint_at_cursor(); }
            return;
        }
        // Any motion re-reveals the transport.
        self.since_pointer = 0.0;
        if self.scrubbing {
            self.scrub_to(x);
            self.cursor = (x, y);
            return;
        }
        if let Some((lx, ly)) = self.drag
            && self.zoom > 1.001
        {
            let base = self.gesture_base(x, y);
            self.center.0 -= (x - lx) / (base.w * self.zoom);
            self.center.1 -= (y - ly) / (base.h * self.zoom);
            self.clamp_center();
            self.drag = Some((x, y));
        }
        self.cursor = (x, y);
    }

    pub fn mouse_down(&mut self, x: f32, y: f32) {
        if self.mask_mode {
            self.cursor = (x, y);
            self.cursor_inside = true;
            self.painting = self.brush_cursor_visible();
            self.stroke_last = None;
            if self.painting { self.paint_at_cursor(); }
            return;
        }
        self.since_pointer = 0.0;
        // While the transport is up, its controls take the press: the
        // buttons act, the seek band scrubs. Anything else pans.
        if self.show_ui && self.ready() && self.transport_alpha() > 0.5 {
            let hit = |r: RectPx| x >= r.x && x <= r.x + r.w && y >= r.y && y <= r.y + r.h;
            if hit(self.btn_prev()) {
                self.step(-1);
                return;
            }
            if hit(self.btn_play()) {
                self.playing = !self.playing;
                return;
            }
            if hit(self.btn_next()) {
                self.step(1);
                return;
            }
            let s = self.seek_rect(self.vp);
            if y >= s.y - SEEK_GRAB && y <= s.y + s.h + SEEK_GRAB && x >= s.x - 6.0
                && x <= s.x + s.w + 6.0
            {
                self.scrubbing = true;
                self.scrub_to(x);
                return;
            }
        }
        self.drag = Some((x, y));
    }

    pub fn mouse_up(&mut self) {
        self.painting = false;
        self.stroke_last = None;
        self.drag = None;
        self.scrubbing = false;
    }

    /// Seek to the position the pointer names on the seek bar.
    fn scrub_to(&mut self, x: f32) {
        if !self.wrap.is_finite() || self.wrap <= 0.0 {
            return;
        }
        let s = self.seek_rect(self.vp);
        let f = ((x - s.x) / s.w.max(1.0)).clamp(0.0, 1.0) as f64;
        self.seek_all(f * (self.wrap - 0.05).max(0.0), true);
    }

    pub fn scroll(&mut self, dx: f32, dy: f32) {
        self.stroke_last = None;
        if self.zoom > 1.001 {
            let base = self.gesture_base(self.cursor.0, self.cursor.1);
            self.center.0 -= dx / (base.w * self.zoom);
            self.center.1 -= dy / (base.h * self.zoom);
            self.clamp_center();
        }
    }

    /// Trackpad pinch, photo-style: scale around the pointer so whatever
    /// sits under it stays put, and every stream shares the resulting
    /// center. Positive delta = fingers spreading = zoom in.
    pub fn pinch(&mut self, delta: f32) {
        self.stroke_last = None;
        if !self.ready() {
            return;
        }
        let (cx, cy) = self.cursor;
        let base = self.gesture_base(cx, cy);
        let old = self.zoom;
        let new = (old * (1.0 + delta)).clamp(1.0, 32.0);
        if (new - old).abs() < 1e-5 {
            return;
        }
        // Content point currently under the pointer…
        let r = Self::zoomed(base, old, self.center);
        let u = ((cx - r.x) / r.w).clamp(0.0, 1.0);
        let v = ((cy - r.y) / r.h).clamp(0.0, 1.0);
        // …stays under it at the new scale.
        let bcx = base.x + base.w / 2.0;
        let bcy = base.y + base.h / 2.0;
        self.zoom = new;
        self.center.0 = u + (bcx - cx) / (base.w * new);
        self.center.1 = v + (bcy - cy) / (base.h * new);
        self.clamp_center();
    }

    fn clamp_center(&mut self) {
        // Keep the view inside the content; at zoom 1 this pins (0.5, 0.5).
        let half = 0.5 / self.zoom.max(1.0);
        self.center.0 = self.center.0.clamp(half, 1.0 - half);
        self.center.1 = self.center.1.clamp(half, 1.0 - half);
    }

    fn fit_rect(dims: (u32, u32), c: RectPx) -> RectPx {
        let (w, h) = (dims.0 as f32, dims.1 as f32);
        let s = (c.w / w).min(c.h / h);
        let (fw, fh) = (w * s, h * s);
        RectPx { x: c.x + (c.w - fw) / 2.0, y: c.y + (c.h - fh) / 2.0, w: fw, h: fh }
    }

    /// The fit rect scaled by `zoom` with content point `center` at the
    /// base rect's middle — the one transform every video shares.
    fn zoomed(base: RectPx, zoom: f32, center: (f32, f32)) -> RectPx {
        let (w, h) = (base.w * zoom, base.h * zoom);
        RectPx {
            x: base.x + base.w / 2.0 - center.0 * w,
            y: base.y + base.h / 2.0 - center.1 * h,
            w,
            h,
        }
    }

    /// The base fit rect a pointer gesture at (x, y) is anchored to: the
    /// hovered cell in side-by-side, the active video's full-window fit
    /// otherwise.
    fn gesture_base(&self, x: f32, _y: f32) -> RectPx {
        let n = self.videos.len();
        if self.mode == Mode::SideBySide && !self.mask_mode {
            let cw = (self.vp.0 / n as f32).max(1.0);
            let i = ((x / cw) as usize).min(n - 1);
            let dims = (self.videos[i].info.width, self.videos[i].info.height);
            Self::fit_rect(dims, RectPx { x: i as f32 * cw, y: 0.0, w: cw, h: self.vp.1 })
        } else {
            let dims = (self.videos[self.active].info.width, self.videos[self.active].info.height);
            Self::fit_rect(dims, RectPx { x: 0.0, y: 0.0, w: self.vp.0, h: self.vp.1 })
        }
    }

    fn content_rect(&self, idx: usize) -> RectPx {
        let dims = (self.videos[idx].info.width, self.videos[idx].info.height);
        let base = Self::fit_rect(dims, RectPx { x: 0.0, y: 0.0, w: self.vp.0, h: self.vp.1 });
        Self::zoomed(base, self.zoom, self.center)
    }

    pub fn tick(&mut self, dt: f32, vp: (f32, f32), _scale: f32) -> FrameDesc {
        if self.vp != vp { self.stroke_last = None; }
        self.vp = vp;
        if let Some(receiver) = &self.mask_save {
            match receiver.try_recv() {
                Ok(result) => {
                    self.mask_status = match result {
                        Ok(path) => format!("Saved {}", path.file_name().unwrap_or_default().to_string_lossy()),
                        Err(error) => { log::error!("Mask save failed: {error}"); format!("Save failed: {error}") }
                    };
                    self.mask_save = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.mask_status = "Save failed: worker stopped".into();
                    self.mask_save = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        // Nothing loaded — paint the launch window and stop.
        if !self.ready() {
            return self.launch_frame(vp);
        }
        let n = self.videos.len();
        let full_uv = [0.0, 0.0, 1.0, 1.0];

        self.since_pointer += dt;
        if self.playing && self.started {
            self.t += dt as f64 * self.speed;
            if self.wrap.is_finite() && self.t >= self.wrap - 0.05 {
                self.seek_all(0.0, true);
            }
        }

        // Drain frames against the master clock.
        let mut uploads = Vec::new();
        let t = self.t;
        let active = self.active;
        let mut adopt = None;
        for (i, v) in self.videos.iter_mut().enumerate() {
            let got = if v.pending {
                let g = v.player.take_next();
                if g.is_some() {
                    v.pending = false;
                }
                g
            } else {
                v.player.take_upto(t + 1e-6)
            };
            if let Some((pts, buf)) = got {
                if v.pending == false && i == active && !self.playing {
                    adopt = Some(pts);
                }
                v.shown_pts = pts;
                v.delivered = true;
                uploads.push(Upload { idx: i, w: v.player.w, h: v.player.h, buf });
            }
        }
        // While paused, glue the clock to the active stream's delivered
        // frame (float-safe framestep adoption).
        if !self.playing && let Some(pts) = adopt {
            self.t = pts;
        }
        if !self.started {
            self.started = self.videos.iter().all(|v| v.delivered || v.player.failed());
        }
        self.badge_flash = (self.badge_flash - dt).max(0.0);

        // ---- draw list ----
        let mut items = Vec::new();
        let a = self.active;
        let b = (self.active + 1) % n;
        let mode = if self.mask_mode { Mode::Overlay } else { self.shown_mode() };
        match mode {
            Mode::Overlay => items.push(Item::Video {
                a,
                b: a,
                r: self.content_rect(a),
                uv: full_uv,
                mode: VideoMode::Tex,
                p0: 0.0,
                p1: 0.0,
            }),
            Mode::SideBySide => {
                let cw = vp.0 / n as f32;
                for i in 0..n {
                    let cell = RectPx { x: i as f32 * cw, y: 0.0, w: cw, h: vp.1 };
                    let dims = (self.videos[i].info.width, self.videos[i].info.height);
                    // Every cell shares the zoom/center, so panning one
                    // pans them all to the same content position.
                    let base = Self::fit_rect(dims, cell);
                    items.push(Item::Video {
                        a: i,
                        b: i,
                        r: Self::zoomed(base, self.zoom, self.center),
                        uv: full_uv,
                        mode: VideoMode::Tex,
                        p0: 0.0,
                        p1: 0.0,
                    });
                }
            }
            Mode::Delta | Mode::Split | Mode::Checker | Mode::Blend => {
                let r = self.content_rect(a);
                let (mode, p0) = match self.mode {
                    Mode::Delta => (VideoMode::Delta, 0.0),
                    // Divider follows the pointer, in the (possibly
                    // zoomed) rect's own coordinates.
                    Mode::Split => {
                        (VideoMode::Split, ((self.cursor.0 - r.x) / r.w.max(1.0)).clamp(0.0, 1.0))
                    }
                    Mode::Checker => (VideoMode::Checker, self.checker_px),
                    _ => (VideoMode::Blend, self.blend),
                };
                items.push(Item::Video { a, b, r, uv: full_uv, mode, p0, p1: self.gain });
            }
        }

        // Big letter badge: hugs the left edge for A, right for B (a >2
        // set interpolates across). Always on with the UI up, a brief
        // flash after Enter when it's hidden.
        let badge_alpha = if self.show_ui { 1.0 } else { (self.badge_flash / 0.4).min(1.0) };
        if badge_alpha > 0.0 && !self.mask_mode {
            // The design sizes the badge against the frame (130px on a
            // 506px-tall mock); keep that proportion so it stays the
            // dominant graphic at any window size.
            let big = (vp.1 * 0.257).clamp(72.0, 240.0);
            let push_letter = |items: &mut Vec<Item>, idx: usize, px: f32, on: bool| {
                let f = if n > 1 { idx as f32 / (n - 1) as f32 } else { 0.0 };
                let margin = 26.0;
                let mut c = if on { ACCENT } else { INACTIVE };
                c[3] *= badge_alpha * 0.92;
                let align = if f < 0.5 { Align::Left } else { Align::Right };
                let x = margin + f * (vp.0 - 2.0 * margin);
                let letter = ((b'A' + idx as u8) as char).to_string();
                // Soft drop shadow so the badge holds against bright
                // footage (the design's text-shadow).
                items.push(Item::Text(TextItem {
                    align,
                    valign: VAlign::Middle,
                    ..TextItem::new(
                        x + px * 0.02,
                        vp.1 / 2.0 + px * 0.03,
                        px,
                        [0.0, 0.0, 0.0, 0.45 * badge_alpha],
                        letter.clone(),
                    )
                }));
                items.push(Item::Text(TextItem {
                    align,
                    valign: VAlign::Middle,
                    ..TextItem::new(x, vp.1 / 2.0, px, c, letter)
                }));
            };
            match mode {
                Mode::Overlay => push_letter(&mut items, a, big, true),
                // Comparing a pair: both letters, active in the accent.
                Mode::Delta | Mode::Split | Mode::Checker | Mode::Blend => {
                    push_letter(&mut items, a, big * 0.74, true);
                    push_letter(&mut items, b, big * 0.74, false);
                }
                // One label per cell, sitting over its own video.
                Mode::SideBySide => {
                    let cw = vp.0 / n as f32;
                    for i in 0..n {
                        let mut c = if i == a { ACCENT } else { INACTIVE };
                        c[3] *= badge_alpha;
                        items.push(Item::Text(TextItem {
                            valign: VAlign::Middle,
                            ..TextItem::new(
                                i as f32 * cw + 20.0,
                                vp.1 / 2.0,
                                big * 0.42,
                                c,
                                ((b'A' + i as u8) as char).to_string(),
                            )
                        }));
                    }
                }
            }
        }

        if self.mask_mode {
            self.build_mask_layer(&mut items);
            self.build_status_line(&mut items, vp);
        } else if self.show_ui {
            self.build_hud(&mut items, vp);
            self.build_status_line(&mut items, vp);
        }

        let animating =
            self.playing || self.badge_flash > 0.0 || !self.started || self.hud_fading();
        FrameDesc {
            clear: FRAME_BG,
            uploads,
            items,
            animating,
            redraw_at: if animating {
                None
            } else {
                Some(Instant::now() + std::time::Duration::from_millis(100))
            },
        }
    }

    /// True while the transport is mid-fade (keeps the loop hot just
    /// long enough for the reveal to finish).
    fn hud_fading(&self) -> bool {
        let a = self.transport_alpha();
        a > 0.001 && a < 0.999
    }

    /// The transport is hover-revealed (per the design): pointer motion
    /// brings it up, then it fades out after a spell of stillness.
    /// Scrubbing pins it open.
    fn transport_alpha(&self) -> f32 {
        if self.scrubbing {
            return 1.0;
        }
        let over = self.since_pointer - TRANSPORT_HOLD_S;
        if over <= 0.0 {
            1.0
        } else {
            (1.0 - over / TRANSPORT_FADE_S).clamp(0.0, 1.0)
        }
    }

    /// Bottom transport strip geometry, shared by the draw and the
    /// seek-bar hit test so they can't drift.
    fn transport_rect(&self, vp: (f32, f32)) -> RectPx {
        RectPx { x: 0.0, y: vp.1 - TRANSPORT_H, w: vp.0, h: TRANSPORT_H }
    }

    /// Transport button hit/draw rects — shared by the draw and the
    /// click handler so a button can't move out from under its target.
    fn btn_prev(&self) -> RectPx {
        let bar = self.transport_rect(self.vp);
        RectPx { x: 22.0, y: bar.y + 13.0, w: 26.0, h: 32.0 }
    }
    fn btn_play(&self) -> RectPx {
        let bar = self.transport_rect(self.vp);
        RectPx { x: 22.0 + 26.0 + 8.0, y: bar.y + 13.0, w: 32.0, h: 32.0 }
    }
    fn btn_next(&self) -> RectPx {
        let bar = self.transport_rect(self.vp);
        RectPx { x: 22.0 + 26.0 + 8.0 + 32.0 + 8.0, y: bar.y + 13.0, w: 26.0, h: 32.0 }
    }

    /// The seek bar's drawn track (the clickable band is taller).
    fn seek_rect(&self, vp: (f32, f32)) -> RectPx {
        let bar = self.transport_rect(vp);
        let x0 = self.btn_next().x + self.btn_next().w + 13.0;
        let x1 = (vp.0 - 22.0 - STATUS_RESERVE).max(x0 + 40.0);
        RectPx { x: x0, y: bar.y + 13.0 + 16.0 - 2.5, w: x1 - x0, h: 5.0 }
    }

    /// The 2a HUD: corner brackets, centre A|B toggle, top-left info
    /// block, and the hover-revealed transport (circular buttons,
    /// rounded seek bar, keycap row).
    fn build_hud(&mut self, items: &mut Vec<Item>, vp: (f32, f32)) {
        let (w, h) = vp;
        let a = self.active;
        let n = self.videos.len();

        // ---- corner brackets framing the active stream ----
        let (inset, arm, t) = (16.0, 26.0, 2.0);
        let top = inset + self.top_inset();
        for (hx, hy, vx, vy) in [
            (inset, top, inset, top),
            (w - inset - arm, top, w - inset - t, top),
            (inset, h - inset - t, inset, h - inset - arm),
            (w - inset - arm, h - inset - t, w - inset - t, h - inset - arm),
        ] {
            items.push(Item::Rect(RectItem::new(
                RectPx { x: hx, y: hy, w: arm, h: t },
                ACCENT,
            )));
            items.push(Item::Rect(RectItem::new(
                RectPx { x: vx, y: vy, w: t, h: arm },
                ACCENT,
            )));
        }

        // ---- centre A|B toggle ----
        // Sits IN the titlebar strip, level with the traffic lights (the
        // strip is otherwise empty past them); fake fullscreen has no strip.
        let (seg_w, seg_h, seg_gap, pill_pad) = (38.0, 18.0, 2.0, 3.0);
        let pill_w = n as f32 * seg_w + (n as f32 - 1.0) * seg_gap + pill_pad * 2.0;
        let pill = RectPx {
            x: (w - pill_w) / 2.0,
            y: if self.fullscreen { 10.0 } else { (TITLEBAR_H - seg_h - pill_pad * 2.0) / 2.0 },
            w: pill_w,
            h: seg_h + pill_pad * 2.0,
        };
        items.push(Item::Rect(RectItem {
            radius: 7.0,
            border_w: 1.0,
            border_color: ACCENT_EDGE,
            ..RectItem::new(pill, PILL_BG)
        }));
        for i in 0..n {
            let sx = pill.x + pill_pad + i as f32 * (seg_w + seg_gap);
            let on = i == a;
            if on {
                items.push(Item::Rect(RectItem {
                    radius: 4.5,
                    ..RectItem::new(
                        RectPx { x: sx, y: pill.y + pill_pad, w: seg_w, h: seg_h },
                        ACCENT,
                    )
                }));
            }
            items.push(Item::Text(TextItem {
                align: Align::Center,
                valign: VAlign::Middle,
                ..TextItem::new(
                    sx + seg_w / 2.0,
                    pill.y + pill.h / 2.0,
                    11.0,
                    if on { FRAME_INK } else { SEG_OFF },
                    ((b'A' + i as u8) as char).to_string(),
                )
            }));
        }

        // ---- top-left info block ----
        let mut y = 22.0 + self.top_inset();
        for (i, v) in self.videos.iter().enumerate() {
            let on = i == a;
            let name = ellipsize(
                &v.info
                    .path
                    .file_name()
                    .map(|f| f.to_string_lossy().into_owned())
                    .unwrap_or_default(),
                34,
            );
            let name_len = name.chars().count();
            // Title row: dark strip with a coloured left rule.
            let row_h = 19.0;
            let row_w = INFO_W;
            items.push(Item::Rect(RectItem::new(
                RectPx { x: 22.0, y, w: row_w, h: row_h },
                if on { ROW_BG_ON } else { ROW_BG_OFF },
            )));
            items.push(Item::Rect(RectItem::new(
                RectPx { x: 22.0, y, w: 2.0, h: row_h },
                if on { ACCENT } else { RULE_OFF },
            )));
            let letter_c = if on { ACCENT } else { INACTIVE };
            items.push(Item::Text(TextItem {
                valign: VAlign::Middle,
                ..TextItem::new(
                    30.0,
                    y + row_h / 2.0,
                    11.0,
                    letter_c,
                    ((b'A' + i as u8) as char).to_string(),
                )
            }));
            items.push(Item::Text(TextItem {
                valign: VAlign::Middle,
                ..TextItem::new(
                    44.0,
                    y + row_h / 2.0,
                    12.0,
                    if on { TEXT } else { TEXT_OFF },
                    name,
                )
            }));
            if on {
                // Sits inline after the filename (monospace step), so it
                // reads as part of the title rather than floating right.
                let after = 44.0 + name_len as f32 * 12.0 * MONO_ADV + 10.0;
                items.push(Item::Text(TextItem {
                    valign: VAlign::Middle,
                    tracking: 1.2,
                    ..TextItem::new(
                        after.min(22.0 + row_w - 60.0),
                        y + row_h / 2.0,
                        9.0,
                        ACCENT,
                        "● SHOWN",
                    )
                }));
            }
            y += row_h + 2.0;

            // Detail lines ride their own faint strips: the design's dark
            // mock stays legible bare, but real footage can be bright
            // anywhere, and this is where you read the numbers.
            let detail = |items: &mut Vec<Item>, y: f32, col: [f32; 4], s: String| {
                items.push(Item::Rect(RectItem::new(
                    RectPx { x: 22.0, y: y - 2.0, w: INFO_W, h: 15.0 },
                    DETAIL_BG,
                )));
                items.push(Item::Text(TextItem::new(30.0, y, 10.5, col, ellipsize(&s, INFO_CH))));
            };

            let failed = v.player.failed();
            if failed {
                detail(items, y, ERR, "DECODE FAILED".into());
                y += 21.0;
            } else {
                let br = v
                    .info
                    .bit_rate
                    .map(|b| format!("{:.2} Mb/s", b as f64 / 1e6))
                    .unwrap_or_else(|| "? Mb/s".into());
                detail(
                    items,
                    y,
                    DETAIL,
                    format!(
                        "{}×{}  {:.3} fps  {} {}",
                        v.info.width, v.info.height, v.info.fps, v.info.codec, v.info.pix_fmt
                    ),
                );
                y += 15.0;
                detail(
                    items,
                    y,
                    DIM,
                    format!(
                        "{}  {}  {}",
                        br,
                        fmt_size(v.info.file_size),
                        fmt_time(v.info.duration)
                    ),
                );
                y += 15.0;
                // Path last, truncated from the LEFT — the leaf directory
                // is what tells two encodes apart.
                let dir = v
                    .info
                    .path
                    .parent()
                    .map(|p| p.to_string_lossy().into_owned())
                    .unwrap_or_default();
                detail(items, y, DIM_PATH, ellipsize_left(&dir, INFO_CH));
                y += 17.0;
            }
            y += 4.0;
        }

        // ---- transport (hover-revealed) ----
        let alpha = self.transport_alpha();
        if alpha <= 0.001 {
            return;
        }
        let fade = |mut c: [f32; 4]| {
            c[3] *= alpha;
            c
        };
        let bar = self.transport_rect(vp);
        // The scrim reaches well above the controls so its weak upper
        // end lands on empty frame, not on the seek bar.
        items.push(Item::Rect(RectItem {
            fade_up: true,
            ..RectItem::new(
                RectPx { y: bar.y - SCRIM_LEAD, h: bar.h + SCRIM_LEAD, ..bar },
                fade(SCRIM),
            )
        }));
        items.push(Item::Rect(RectItem::new(
            RectPx { x: bar.x, y: bar.y, w: bar.w, h: 1.0 },
            fade(TRANSPORT_RULE),
        )));

        let cy = bar.y + 13.0 + 16.0;
        // Prev / play-pause / next — the middle one on an accent disc.
        // All drawn as geometry (`Item::Triangle` + rects): the system mono
        // fonts lack ⏮/⏸/⏭, and a font's ▶ is placed by its metrics, not
        // its ink, so it never lands in the middle of the disc.
        let prev = self.btn_prev();
        let next = self.btn_next();
        let disc = self.btn_play();
        let tri = |items: &mut Vec<Item>, r: RectPx, left: bool, c: [f32; 4]| {
            items.push(Item::Triangle { r, color: c, left, radius: 1.0 });
        };
        // Skip glyphs: two small triangles nose to tail, centred.
        let (sw, sh) = (7.0, 9.0);
        for (b, left) in [(prev, true), (next, false)] {
            let x0 = b.x + b.w / 2.0 - sw;
            for i in 0..2 {
                tri(items, RectPx { x: x0 + i as f32 * sw, y: cy - sh / 2.0, w: sw, h: sh },
                    left, fade(GLYPH));
            }
        }
        items.push(Item::Rect(RectItem {
            radius: disc.w / 2.0,
            ..RectItem::new(disc, fade(ACCENT))
        }));
        let dcx = disc.x + disc.w / 2.0;
        if self.playing {
            for dx in [-4.5, 1.5] {
                items.push(Item::Rect(RectItem {
                    radius: 1.0,
                    ..RectItem::new(
                        RectPx { x: dcx + dx, y: cy - 6.0, w: 3.0, h: 12.0 },
                        fade(FRAME_INK),
                    )
                }));
            }
        } else {
            // Same 12px height as the pause bars (plus the rounding), and
            // nudged right of the box centre toward the centroid — a
            // box-centred triangle reads as sitting left in a circle.
            let (tw, th) = (PLAY_W, PLAY_H);
            tri(items, RectPx { x: dcx - tw * 0.42, y: cy - th / 2.0, w: tw, h: th },
                false, fade(FRAME_INK));
        }

        // Seek bar: track, accent fill to the playhead, white knob.
        let seek = self.seek_rect(vp);
        let frac = if self.wrap.is_finite() && self.wrap > 0.0 {
            (self.t / self.wrap).clamp(0.0, 1.0) as f32
        } else {
            0.0
        };
        items.push(Item::Rect(RectItem {
            radius: 3.0,
            ..RectItem::new(seek, fade(TRACK))
        }));
        if frac > 0.0 {
            items.push(Item::Rect(RectItem {
                radius: 3.0,
                ..RectItem::new(RectPx { w: seek.w * frac, ..seek }, fade(ACCENT))
            }));
        }
        items.push(Item::Rect(RectItem {
            radius: 6.5,
            ..RectItem::new(
                RectPx {
                    x: seek.x + seek.w * frac - 6.5,
                    y: seek.y + seek.h / 2.0 - 6.5,
                    w: 13.0,
                    h: 13.0,
                },
                fade(KNOB),
            )
        }));

        // Status readout, right-aligned inside the reserved strip.
        let mode = self.shown_mode();
        let extra = match mode {
            Mode::Delta => format!("  gain ×{:.1}", self.gain),
            Mode::Blend => format!("  blend {:.0}%", self.blend * 100.0),
            Mode::Checker => format!("  checker {:.0}px", self.checker_px),
            _ => String::new(),
        };
        let speed = if (self.speed - 1.0).abs() > 1e-3 {
            format!(" · {:.2}×", self.speed)
        } else {
            String::new()
        };
        let zoom = if self.zoom > 1.001 {
            format!("{:.1}×", self.zoom)
        } else {
            "fit".to_string()
        };
        let mut sx = w - 22.0;
        for (txt, col) in [
            (zoom, fade(DIM)),
            (
                format!(
                    "· frame {}{} ·",
                    (self.t * self.fps).round() as i64,
                    speed
                ),
                fade(DIM),
            ),
            (
                format!(
                    "/ {}",
                    if self.wrap.is_finite() { fmt_time(self.wrap) } else { "?".into() }
                ),
                fade(TIME_OFF),
            ),
            (fmt_time(self.t), fade(TEXT)),
            (format!("{}{}", mode.name(), extra), fade(ACCENT)),
        ] {
            items.push(Item::Text(TextItem {
                align: Align::Right,
                valign: VAlign::Middle,
                ..TextItem::new(sx, cy, 11.0, col, txt.clone())
            }));
            // Right-to-left walk; monospace so a per-char step is exact.
            sx -= txt.chars().count() as f32 * 11.0 * MONO_ADV + 8.0;
        }

    }

    /// The bottom status line, vim/helix style: a fixed-width chip naming
    /// the input mode on the left, that mode's keys beside it, mode status
    /// on the right. The bar is tinted by mode (dark blue A/B, dark red
    /// mask) so the mode reads before the chip does. Unlike the transport
    /// above it, it never fades — it is how you tell which keys are live —
    /// and only Tab (A/B) hides it.
    fn build_status_line(&self, items: &mut Vec<Item>, vp: (f32, f32)) {
        let bar = RectPx { x: 0.0, y: vp.1 - STATUS_H, w: vp.0, h: STATUS_H };
        let cy = bar.y + bar.h / 2.0;
        let (bg, chip, name) = if self.mask_mode {
            (STATUS_BG_MASK, MASK_RED, "MASK")
        } else {
            (STATUS_BG_AB, ACCENT, "A/B TEST")
        };
        items.push(Item::Rect(RectItem::new(bar, bg)));
        items.push(Item::Rect(RectItem::new(
            RectPx { x: 0.0, y: bar.y, w: MODE_W, h: bar.h },
            chip,
        )));
        items.push(Item::Text(TextItem {
            align: Align::Center,
            valign: VAlign::Middle,
            tracking: 1.5,
            ..TextItem::new(MODE_W / 2.0, cy, 11.0, TEXT, name)
        }));

        let clips = match self.videos.len() {
            1 => "1".to_string(),
            n => format!("1-{}", n.min(9)),
        };
        let keys: &[(&str, &str)] = if self.mask_mode {
            &[
                ("drag", "paint"),
                ("+ -", "brush"),
                ("s", "save"),
                (&clips, "clip"),
                ("m", "exit"),
                ("enter", "next clip"),
            ]
        } else {
            &[
                (&clips, "clip"),
                ("space", "play"),
                ("< >", "frame-step"),
                ("[ ]", "speed"),
                ("v", "view"),
                ("m", "mask"),
                ("tab", "info"),
                ("f", "fullscreen"),
                ("enter", "flip"),
            ]
        };

        // Right-hand status first, so the keycaps stop short of it.
        let status = if self.mask_mode {
            let v = &self.videos[self.active];
            let name = v.info.path.file_name().unwrap_or_default().to_string_lossy();
            let tail = if self.mask_status.is_empty() {
                "blue → white · red → black"
            } else {
                &self.mask_status
            };
            format!(
                "{} {} · brush {:.0}px · {}",
                (b'A' + self.active as u8) as char,
                ellipsize(&name, 28),
                self.brush_diameter,
                tail
            )
        } else {
            String::new()
        };
        let status_w = status.chars().count() as f32 * 11.0 * MONO_ADV;
        let limit = vp.0 - 16.0 - if status_w > 0.0 { status_w + 24.0 } else { 0.0 };
        if !status.is_empty() {
            items.push(Item::Text(TextItem {
                align: Align::Right,
                valign: VAlign::Middle,
                ..TextItem::new(vp.0 - 16.0, cy, 11.0, LABEL, status)
            }));
        }

        // Caps and their key+drop sit centred in the bar together.
        let cap_y = bar.y + (bar.h - CAP_H - CAP_DROP) / 2.0;
        let mut kx = MODE_W + 14.0;
        for &(cap, label) in keys {
            let cap_w = cap_width(cap);
            let step = cap_w + 8.0 + label.chars().count() as f32 * KEY_LABEL_PX * MONO_ADV;
            if kx + step > limit {
                break;
            }
            keycap(items, kx, cap_y, cap);
            items.push(Item::Text(TextItem {
                valign: VAlign::Middle,
                ..TextItem::new(kx + cap_w + 8.0, cap_y + CAP_H / 2.0, KEY_LABEL_PX, TEXT, label)
            }));
            kx += step + 20.0;
        }
    }

    /// The launch window: the wordmark and one line asking for a clip.
    /// (2b's A/B drop targets, terminal hint and keycap legend are gone
    /// for now — one clip already plays, so there is no half-filled pair
    /// to explain.) No video items, so it renders with zero streams loaded.
    fn launch_frame(&self, vp: (f32, f32)) -> FrameDesc {
        let (w, h) = vp;
        let mut items: Vec<Item> = Vec::new();

        // The logo image, not type: it already carries the "VIDEO QUALITY
        // TESTING TOOLKIT" line. Width is capped against the window so a
        // narrow one doesn't run it edge to edge; the renderer owns the
        // aspect, the way it owns glyph metrics. Logo + message are
        // centred in the window as one group.
        let lw = (w * 0.34).clamp(240.0, 460.0).min(w - 96.0);
        let lh = lw / self.logo_aspect;
        let (gap, mpx) = (36.0, 13.0);
        let top = (h - (lh + gap + mpx)) / 2.0;
        items.push(Item::Logo {
            r: RectPx { x: (w - lw) / 2.0, y: top, w: lw, h: lh },
            alpha: 1.0,
        });
        // A drag over the window lights the line — the whole window is
        // the target (winit gives no drop position anyway).
        let (msg, col) = if self.drag_hover {
            ("release to open", ACCENT)
        } else {
            ("drop a video file to begin", DIM)
        };
        items.push(Item::Text(TextItem {
            align: Align::Center,
            ..TextItem::new(w / 2.0, top + lh + gap, mpx, col, msg)
        }));

        FrameDesc {
            clear: LAUNCH_BG,
            uploads: Vec::new(),
            items,
            animating: false,
            redraw_at: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{Duration, Instant};

    #[test]
    fn number_keys_pick_clips_and_v_cycles_views() {
        let Some(clip) = test_clip() else { return; };
        let mut app = mk_app(&clip, 2);
        app.key(Key::Char('2'));
        assert_eq!(app.active, 1);
        app.key(Key::Char('3')); // no third clip: ignored
        assert_eq!(app.active, 1);
        app.key(Key::Char('1'));
        assert_eq!(app.active, 0);
        assert_eq!(app.mode, Mode::Overlay, "number keys no longer pick views");
        app.key(Key::Char('v'));
        assert_eq!(app.mode, Mode::SideBySide);
        app.key(Key::Char('V'));
        app.key(Key::Char('V'));
        assert_eq!(app.mode, Mode::Blend, "Shift-V wraps backwards");
    }

    #[test]
    fn mask_painting_tracks_zoom_focus_and_export() {
        let Some(clip) = test_clip() else { return; };
        let mut app = mk_app(&clip, 2);
        app.mode = Mode::SideBySide;
        app.key(Key::Char('m'));
        assert!(!app.playing);
        assert!(app.mask_mode);
        app.brush_diameter = 8.0;
        app.mouse_down(640.0, 790.0); // bottom letterbox
        app.mouse_up();
        assert_eq!(app.masks[0].as_ref().unwrap().revision, 0);
        // In a 1280x800 viewport the 320x180 video is 1280x720 at y=40.
        app.mouse_down(640.0, 400.0);
        app.cursor_moved(800.0, 400.0);
        app.mouse_up();
        let mask = app.masks[0].as_ref().unwrap();
        assert_eq!(mask.pixels[90 * 320 + 160], 255);
        assert_eq!(mask.pixels[90 * 320 + 180], 255);
        assert_eq!(mask.pixels[90 * 320 + 200], 255);
        assert_eq!(mask.pixels[70 * 320 + 180], 0);
        // Pinch in mask mode must use the full-window fit, not the SBS cell.
        app.cursor_moved(640.0, 400.0);
        app.pinch(1.0);
        assert_eq!(app.center, (0.5, 0.5));
        app.mouse_down(640.0, 560.0);
        app.mouse_up();
        assert_eq!(app.masks[0].as_ref().unwrap().pixels[110 * 320 + 160], 255);
        // The status line is not a paint target (zoomed, video covers it).
        let revision = app.masks[0].as_ref().unwrap().revision;
        app.mouse_down(300.0, 790.0);
        app.mouse_up();
        assert_eq!(app.masks[0].as_ref().unwrap().revision, revision);
        app.key(Key::Char('+'));
        assert!(app.brush_diameter > 8.0);
        app.key(Key::Char('-'));
        assert!((app.brush_diameter - 8.0).abs() < 0.01);
        app.key(Key::Enter);
        assert!(app.masks[1].as_ref().unwrap().pixels.iter().all(|p| *p == 0));
        app.mouse_down(640.0, 400.0);
        app.cursor_left();
        assert!(!app.painting);
        assert!(!app.brush_cursor_visible());
        let output_video = std::env::temp_dir().join(format!("abner-focused-{}.mp4", std::process::id()));
        app.videos[1].info.path = output_video.clone();
        app.key(Key::Char('s'));
        assert!(tick_until(&mut app, Duration::from_secs(2), |a| a.mask_save.is_none()));
        assert!(app.mask_status.starts_with("Saved"), "{}", app.mask_status);
        let output = mask::output_path(&output_video);
        assert!(output.exists());
        std::fs::remove_file(output).unwrap();
        app.key(Key::Char('m'));
        assert_eq!(app.mode, Mode::SideBySide);
        app.key(Key::Char('m'));
        app.key(Key::Enter);
        assert_eq!(app.masks[0].as_ref().unwrap().revision, revision);
        app.add_videos(vec![mk_video(&clip)], false);
        assert!(app.masks[0].is_some());
        assert!(!app.mask_mode);
        app.add_videos(vec![mk_video(&clip), mk_video(&clip)], true);
        assert!(app.masks.iter().all(Option::is_none));
    }

    fn test_clip() -> Option<PathBuf> {
        if Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("skipping: ffmpeg not on PATH");
            return None;
        }
        let dir = std::env::temp_dir().join("abner_app_test");
        let _ = std::fs::create_dir_all(&dir);
        let clip = dir.join("step.mp4");
        if !clip.exists() {
            let ok = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
                .arg("testsrc2=duration=4:size=320x180:rate=30")
                .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "30"])
                .arg(&clip)
                .status()
                .map(|s| s.success())
                .unwrap_or(false);
            assert!(ok, "failed to generate test clip");
        }
        Some(clip)
    }

    fn mk_video(clip: &PathBuf) -> Video {
        let info = probe::probe(clip).expect("probe");
        let player = crate::player::Player::spawn(
            clip,
            info.width,
            info.height,
            probe::vt_accel(&info.codec),
            info.rotation,
        )
        .expect("spawn");
        Video { info, player, shown_pts: 0.0, delivered: false, pending: false }
    }

    fn mk_app(clip: &PathBuf, n: usize) -> App {
        App::new((0..n).map(|_| mk_video(clip)).collect(), Mode::Overlay)
    }

    /// Tick until a condition holds (real decode runs behind this).
    fn tick_until(app: &mut App, within: Duration, cond: impl Fn(&App) -> bool) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            let desc = app.tick(0.010, (1280.0, 800.0), 2.0);
            for u in desc.uploads {
                app.recycle(u.idx, u.buf);
            }
            if cond(app) {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }


    /// Dropped clips fill slots in order: one file lands in A and plays
    /// on its own, a second makes the pair (both back at 0), and a
    /// further drop appends C. Whatever the
    /// route in, every stream must come back to the same instant — a
    /// stream that kept its old position would be silently unsynced.
    #[test]
    fn dropped_clips_fill_slots_in_order_and_stay_synced() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 0);
        assert!(!app.ready(), "no clips is the launch window");

        // The launch window draws with nothing loaded (no uploads).
        assert!(app.tick(0.016, (1280.0, 800.0), 2.0).uploads.is_empty());

        // One clip: slot A holds it and it plays — in any view mode, since
        // a lone clip has nothing to be compared against.
        app.add_videos(vec![mk_video(&clip)], false);
        assert_eq!(app.videos.len(), 1);
        assert!(app.ready(), "one clip is enough to play");
        app.mode = Mode::Delta;
        assert!(
            tick_until(&mut app, Duration::from_secs(10), |a| a.started && a.t > 0.3),
            "a lone clip never started playing"
        );
        app.mode = Mode::Overlay;

        // A second clip makes the pair, and the clock rewinds for it.
        app.add_videos(vec![mk_video(&clip)], false);
        assert_eq!(app.videos.len(), 2);
        assert_eq!(app.t, 0.0);
        assert!(
            tick_until(&mut app, Duration::from_secs(10), |a| a.started && a.t > 0.3),
            "playback never started after the pair completed"
        );

        // A third dropped onto the running pair appends, and the clock
        // goes back to 0 so the newcomer is locked to the other two.
        app.add_videos(vec![mk_video(&clip)], false);
        assert_eq!(app.videos.len(), 3);
        assert_eq!(app.t, 0.0, "an added stream must rewind everyone");
        assert!(!app.started);
        assert!(
            tick_until(&mut app, Duration::from_secs(10), |a| a.started && a.t > 0.3),
            "playback never resumed after the append"
        );
        let pts: Vec<f64> = app.videos.iter().map(|v| v.shown_pts).collect();
        let spread = pts.iter().cloned().fold(f64::MIN, f64::max)
            - pts.iter().cloned().fold(f64::MAX, f64::min);
        assert!(spread < 1.5 / app.fps, "streams drifted after an append: {pts:?}");

        // ⌘-drop replaces the whole set instead of adding to it.
        app.add_videos(vec![mk_video(&clip), mk_video(&clip)], true);
        assert_eq!(app.videos.len(), 2, "replace starts a fresh comparison");
        assert_eq!(app.t, 0.0);
    }

    /// The 2a transport is a real control surface, not a picture of one:
    /// its buttons act and its seek bar scrubs, from the same rects the
    /// draw uses.
    #[test]
    fn transport_buttons_and_seek_bar_are_live() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 2);
        assert!(
            tick_until(&mut app, Duration::from_secs(10), |a| a.started && a.t > 0.2),
            "playback never started"
        );
        let vp = app.vp;

        // Play/pause disc toggles.
        let play = app.btn_play();
        assert!(app.playing);
        app.mouse_down(play.x + play.w / 2.0, play.y + play.h / 2.0);
        app.mouse_up();
        assert!(!app.playing, "clicking the disc should pause");

        // Next steps one frame forward and stays paused.
        let before = app.t;
        let next = app.btn_next();
        app.mouse_down(next.x + next.w / 2.0, next.y + next.h / 2.0);
        app.mouse_up();
        assert!(
            tick_until(&mut app, Duration::from_secs(5), |a| !a.videos.iter().any(|v| v.pending)),
            "step frame never arrived"
        );
        assert!(app.t > before, "next should advance: {before:.4} -> {:.4}", app.t);

        // A press on the seek track scrubs to that fraction, and never
        // starts a pan drag.
        let s = app.seek_rect(vp);
        app.mouse_down(s.x + s.w * 0.75, s.y + s.h / 2.0);
        assert!(app.scrubbing);
        assert!(app.drag.is_none(), "a seek press must not also pan");
        let want = 0.75 * (app.wrap - 0.05);
        assert!(
            (app.t - want).abs() < 0.2,
            "seek should land at ~75%: wanted {want:.3}, got {:.3}",
            app.t
        );
        app.mouse_up();
        assert!(!app.scrubbing);

        // A press on open frame still pans.
        app.mouse_down(vp.0 / 2.0, vp.1 / 2.0);
        assert!(app.drag.is_some());
    }

    /// Photo-style pinch: the content point under the pointer must stay
    /// under it across zooms, every video shares the transform, and Z
    /// resets.
    #[test]
    fn pinch_zooms_around_the_pointer_and_z_resets() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 2);
        let desc = app.tick(0.0, (1280.0, 800.0), 2.0);
        for u in desc.uploads {
            app.recycle(u.idx, u.buf);
        }
        app.cursor_moved(900.0, 300.0);
        let r0 = app.content_rect(0);
        let u0 = ((900.0 - r0.x) / r0.w, (300.0 - r0.y) / r0.h);
        app.pinch(0.5);
        app.pinch(0.5);
        assert!(app.zoom > 1.5, "zoom {}", app.zoom);
        let r1 = app.content_rect(0);
        let u1 = ((900.0 - r1.x) / r1.w, (300.0 - r1.y) / r1.h);
        assert!(
            (u0.0 - u1.0).abs() < 0.01 && (u0.1 - u1.1).abs() < 0.01,
            "content moved under the pointer: {u0:?} -> {u1:?}"
        );
        // Same-dims videos land on identical rects — position stays synced.
        let (ra, rb) = (app.content_rect(0), app.content_rect(1));
        assert!((ra.x - rb.x).abs() < 1e-3 && (ra.y - rb.y).abs() < 1e-3);
        app.key(Key::Char('z'));
        assert!((app.zoom - 1.0).abs() < 1e-6);
        assert_eq!(app.center, (0.5, 0.5));
    }

    /// `[`/`]` scale how fast the master clock runs; Backspace resets.
    #[test]
    fn speed_scales_the_master_clock() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 2);
        assert!(
            tick_until(&mut app, Duration::from_secs(10), |a| a.started && a.t > 0.2),
            "playback never started"
        );
        app.key(Key::Char(']'));
        app.key(Key::Char(']'));
        assert!((app.speed - 1.5625).abs() < 1e-9);
        let t0 = app.t;
        for _ in 0..10 {
            let desc = app.tick(0.010, (1280.0, 800.0), 2.0);
            for u in desc.uploads {
                app.recycle(u.idx, u.buf);
            }
        }
        let advanced = app.t - t0;
        assert!(
            (advanced - 0.10 * 1.5625).abs() < 1e-6,
            "clock should run at 1.5625×: advanced {advanced:.5}"
        );
        app.key(Key::Backspace);
        assert!((app.speed - 1.0).abs() < 1e-9);
    }

    /// Pause, framestep forward twice and back once: the master clock must
    /// land one frame period over from where it started, adopted from the
    /// decoder's true pts (not accumulated float targets).
    #[test]
    fn framestep_moves_one_frame_and_adopts_true_pts() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 2);
        assert!(
            tick_until(&mut app, Duration::from_secs(10), |a| a.started && a.t > 0.2),
            "playback never started"
        );
        app.key(Key::Space); // pause
        assert!(!app.playing);
        assert!(
            tick_until(&mut app, Duration::from_secs(5), |a| !a.videos.iter().any(|v| v.pending)),
            "streams never settled after pause"
        );
        let t0 = app.t;
        let d = 1.0 / 30.0;
        for _ in 0..2 {
            app.key(Key::Char('.'));
            assert!(
                tick_until(&mut app, Duration::from_secs(5), |a| !a
                    .videos
                    .iter()
                    .any(|v| v.pending)),
                "step frame never arrived"
            );
        }
        assert!(
            (app.t - (t0 + 2.0 * d)).abs() < d * 0.6,
            "two steps should advance ~2 frames: t0 {t0:.4} → {:.4}",
            app.t
        );
        app.key(Key::Char(','));
        assert!(
            tick_until(&mut app, Duration::from_secs(5), |a| !a.videos.iter().any(|v| v.pending)),
            "back-step frame never arrived"
        );
        assert!(
            (app.t - (t0 + d)).abs() < d * 0.6,
            "back-step should return to ~1 frame past t0: t0 {t0:.4} → {:.4}",
            app.t
        );
        // Both streams show the same frame after stepping.
        let (a, b) = (app.videos[0].shown_pts, app.videos[1].shown_pts);
        assert!((a - b).abs() < d * 0.5, "steps desynced the streams: {a:.4} vs {b:.4}");
    }
}

// ---- 2a "Instrument HUD, remixed" palette ----
/// Signal accent — the one hot colour in the design, and where the whole
/// palette comes from: 2a's lime read as a different product next to the
/// logo. This is the mark's upper bar (`assets/logo.png`, #006dcf) lifted
/// to #1580de, the one deliberate departure from the file: the bar itself
/// clears only ~4:1 against the HUD's black, which is under the bar for
/// 10–11px mono. Side by side with the mark it still reads as the same
/// blue; illegible status text would not.
const ACCENT: [f32; 4] = [0.082, 0.502, 0.871, 1.0];
/// Hairlines and pill outlines drawn in the accent, well under full.
const ACCENT_EDGE: [f32; 4] = [0.082, 0.502, 0.871, 0.28];
/// Frame background / ink on the accent (#050506). The alpha is the
/// window's: the surface is transparent (`with_transparent` in main.rs),
/// so the desktop shows faintly through the letterbox — opaque video
/// quads are unaffected.
const FRAME_BG: [f32; 4] = [0.0196, 0.0196, 0.0235, 0.92];
const FRAME_INK: [f32; 4] = [0.0196, 0.0196, 0.0235, 1.0];
const TEXT: [f32; 4] = [0.941, 0.941, 0.949, 1.0];
const TEXT_OFF: [f32; 4] = [0.784, 0.784, 0.824, 0.85];
const DETAIL: [f32; 4] = [0.824, 0.824, 0.863, 0.95];
const DIM_PATH: [f32; 4] = [0.588, 0.588, 0.627, 0.75];
const DETAIL_BG: [f32; 4] = [0.0, 0.0, 0.0, 0.82];
const DIM: [f32; 4] = [0.588, 0.588, 0.627, 0.9];
const LABEL: [f32; 4] = [0.784, 0.784, 0.804, 0.85];
const INACTIVE: [f32; 4] = [0.706, 0.706, 0.745, 0.9];
const SEG_OFF: [f32; 4] = [0.902, 0.902, 0.922, 0.8];
const TIME_OFF: [f32; 4] = [0.471, 0.471, 0.510, 0.85];
const GLYPH: [f32; 4] = [0.824, 0.824, 0.843, 0.85];
const ERR: [f32; 4] = [1.0, 0.35, 0.3, 1.0];
const PILL_BG: [f32; 4] = [0.016, 0.016, 0.024, 0.62];
// Panel alphas run high on purpose: see the scrim note in shader.wgsl —
// linear-space blending means 0.6 alpha barely dims bright footage.
const ROW_BG_ON: [f32; 4] = [0.0, 0.0, 0.0, 0.90];
const ROW_BG_OFF: [f32; 4] = [0.0, 0.0, 0.0, 0.82];
const RULE_OFF: [f32; 4] = [1.0, 1.0, 1.0, 0.14];
const SCRIM: [f32; 4] = [0.016, 0.016, 0.024, 0.97];
const TRACK: [f32; 4] = [1.0, 1.0, 1.0, 0.14];
const KNOB: [f32; 4] = [1.0, 1.0, 1.0, 1.0];
/// Status line, tinted by mode and only just see-through: dark blue in
/// A/B (#0b1d3a), dark red in mask (#3a0b10). Mask's chip takes the
/// logo's lower bar (#e71b24), as A/B's takes the upper.
const STATUS_BG_AB: [f32; 4] = [0.043, 0.114, 0.227, 0.93];
const STATUS_BG_MASK: [f32; 4] = [0.227, 0.043, 0.063, 0.93];
const MASK_RED: [f32; 4] = [0.906, 0.106, 0.141, 1.0];
/// The hairline over the transport: faint grey, not the accent.
const TRANSPORT_RULE: [f32; 4] = [1.0, 1.0, 1.0, 0.07];
/// Keycaps — switchblade's design system (`switchblade.toml` [theme] and
/// [theme.keycap], the inline 22px cap): surface, hairline, highlight,
/// shadow and ink are its tokens verbatim.
const KEY_SURFACE: [f32; 4] = [0.043, 0.051, 0.067, 1.0];
/// Brighter than switchblade's 0.11 hairline token: on the tinted status bar
/// the caps need a clearly lit edge.
const KEY_HAIRLINE: [f32; 4] = [1.0, 1.0, 1.0, 0.38];
const KEY_HIGHLIGHT: [f32; 4] = [1.0, 1.0, 1.0, 0.07];
const KEY_SHADOW: [f32; 4] = [0.0, 0.0, 0.0, 0.55];
const KEY_INK: [f32; 4] = [0.957, 0.961, 0.969, 0.9];
const CAP_H: f32 = 22.0;
const CAP_R: f32 = 5.0;
const CAP_FONT: f32 = 13.5;
const CAP_PAD_EM: f32 = 0.58;
const CAP_DROP: f32 = 3.0;
/// The word beside each cap: full ink, a touch larger than the old dim
/// label, so what a key DOES reads as easily as the key.
const KEY_LABEL_PX: f32 = 12.0;

// Launch window.
const LAUNCH_BG: [f32; 4] = [0.027, 0.027, 0.035, 0.90];

/// Info block width, and how many monospace chars fit inside it.
const INFO_W: f32 = 430.0;
const INFO_CH: usize = ((INFO_W - 16.0) / (10.5 * MONO_ADV)) as usize;

/// Transport strip: 13px pad + 32px controls + 11px gap + the status line.
const TRANSPORT_H: f32 = 56.0 + STATUS_H;
/// Bottom status line (mode chip + keycaps), and its mode chip's fixed
/// width — constant across modes, like helix's, so the keys never shift.
const STATUS_H: f32 = 34.0;
const MODE_W: f32 = 92.0;
/// The play triangle, matched to the 12px pause bars it swaps with.
const PLAY_W: f32 = 11.5;
const PLAY_H: f32 = 13.0;
/// Extra scrim drawn above the strip so the gradient's transparent end
/// falls on bare frame rather than on the controls.
const SCRIM_LEAD: f32 = 54.0;
/// Width reserved right of the seek bar for the status readout.
const STATUS_RESERVE: f32 = 330.0;
/// Extra grab margin above/below the 5px seek track.
const SEEK_GRAB: f32 = 9.0;
/// Pointer stillness before the transport starts fading, and the fade.
const TRANSPORT_HOLD_S: f32 = 2.6;
const TRANSPORT_FADE_S: f32 = 0.45;
/// Advance width of the monospace UI font, in em — used only to step
/// between right-aligned status segments and keycap chips, never to
/// place a glyph (the renderer measures those exactly).
const MONO_ADV: f32 = 0.60;

/// Logical height of a standard macOS titlebar. The window has no visible
/// bar (`set_titlebar_glass` makes it transparent and runs the content
/// under it), but the traffic-light buttons still float in that strip, so
/// the top HUD row is pushed below them — everything else goes edge to
/// edge. Zero in fake fullscreen: that window is borderless, buttons and
/// all.
const TITLEBAR_H: f32 = 28.0;

/// Width of a keycap for `label` — switchblade's rule: `pad_em` of air
/// each side of a monospace run, floored at the cap height so a single
/// glyph stays square.
fn cap_width(label: &str) -> f32 {
    (CAP_FONT * CAP_PAD_EM * 2.0 + CAP_FONT * MONO_ADV * label.chars().count() as f32).max(CAP_H)
}

/// One keycap, top-left at `(x, y)`: switchblade's design-system cap
/// (`theme.rs::keycap`, inline size) — a hard drop shadow, a near-black
/// face with a hairline outline, and an inset top highlight held off the
/// corners. That highlight is the whole difference between "a dark
/// rectangle" and "a key".
fn keycap(items: &mut Vec<Item>, x: f32, y: f32, label: &str) {
    let (w, h, r) = (cap_width(label), CAP_H, CAP_R);
    items.push(Item::Rect(RectItem {
        radius: r,
        ..RectItem::new(RectPx { x, y: y + CAP_DROP, w, h }, KEY_SHADOW)
    }));
    items.push(Item::Rect(RectItem {
        radius: r,
        border_w: 1.0,
        border_color: KEY_HAIRLINE,
        ..RectItem::new(RectPx { x, y, w, h }, KEY_SURFACE)
    }));
    let inset = r * 0.55;
    items.push(Item::Rect(RectItem {
        radius: 0.5,
        ..RectItem::new(RectPx { x: x + inset, y: y + 1.0, w: (w - inset * 2.0).max(0.0), h: 1.0 },
            KEY_HIGHLIGHT)
    }));
    items.push(Item::Text(TextItem {
        align: Align::Center,
        valign: VAlign::Middle,
        ..TextItem::new(x + w / 2.0, y + h / 2.0, CAP_FONT, KEY_INK, label)
    }));
}

/// Clip a run to `max` characters, marking the cut with an ellipsis.
fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).collect::<String>() + "…"
}

/// Same, but keeps the TAIL (for paths, where the leaf matters).
fn ellipsize_left(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_string();
    }
    let skip = n - max.saturating_sub(1);
    "…".to_string() + &s.chars().skip(skip).collect::<String>()
}

fn fmt_time(t: f64) -> String {
    let t = t.max(0.0);
    let m = (t / 60.0) as u64;
    format!("{m:02}:{:06.3}", t - m as f64 * 60.0)
}

fn fmt_size(b: u64) -> String {
    const MIB: f64 = 1024.0 * 1024.0;
    let b = b as f64;
    if b >= 1024.0 * MIB {
        format!("{:.2} GiB", b / (1024.0 * MIB))
    } else {
        format!("{:.1} MiB", b / MIB)
    }
}
