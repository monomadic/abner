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
use crate::probe::VideoInfo;
use crate::render::{
    Align, FrameDesc, Item, RectItem, RectPx, TextBg, TextItem, Upload, VAlign, VideoMode,
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
    fn key(self) -> u8 {
        match self {
            Mode::Overlay => 1,
            Mode::SideBySide => 2,
            Mode::Delta => 3,
            Mode::Split => 4,
            Mode::Checker => 5,
            Mode::Blend => 6,
        }
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
        let fps = videos.first().map(|v| v.info.fps).filter(|f| *f > 1.0).unwrap_or(30.0);
        let wrap = videos
            .iter()
            .map(|v| v.info.duration)
            .filter(|d| *d > 0.1)
            .fold(f64::INFINITY, f64::min);
        Self {
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
            cmds: Vec::new(),
        }
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
        // Launch state (no clips loaded): only global keys are live.
        if self.videos.is_empty() {
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
        match k {
            Key::Enter => {
                self.active = (self.active + 1) % self.videos.len();
                self.badge_flash = 1.2;
            }
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
                '1' => self.mode = Mode::Overlay,
                '2' => self.mode = Mode::SideBySide,
                '3' => self.mode = Mode::Delta,
                '4' => self.mode = Mode::Split,
                '5' => self.mode = Mode::Checker,
                '6' => self.mode = Mode::Blend,
                _ => {}
            },
        }
    }

    pub fn cursor_moved(&mut self, x: f32, y: f32) {
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
        self.since_pointer = 0.0;
        // While the transport is up, its controls take the press: the
        // buttons act, the seek band scrubs. Anything else pans.
        if self.show_ui && !self.videos.is_empty() && self.transport_alpha() > 0.5 {
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
        if self.videos.is_empty() {
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
        if self.mode == Mode::SideBySide {
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
        self.vp = vp;
        // No clips loaded yet — paint the launch window (2b) and stop.
        if self.videos.is_empty() {
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
        match self.mode {
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
        if badge_alpha > 0.0 {
            // The design sizes the badge against the frame (130px on a
            // 506px-tall mock); keep that proportion so it stays the
            // dominant graphic at any window size.
            let big = (vp.1 * 0.257).clamp(72.0, 240.0);
            let push_letter = |items: &mut Vec<Item>, idx: usize, px: f32, on: bool| {
                let f = if n > 1 { idx as f32 / (n - 1) as f32 } else { 0.0 };
                let margin = 26.0;
                let mut c = if on { LIME } else { INACTIVE };
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
            match self.mode {
                Mode::Overlay => push_letter(&mut items, a, big, true),
                // Comparing a pair: both letters, active in lime.
                Mode::Delta | Mode::Split | Mode::Checker | Mode::Blend => {
                    push_letter(&mut items, a, big * 0.74, true);
                    push_letter(&mut items, b, big * 0.74, false);
                }
                // One label per cell, sitting over its own video.
                Mode::SideBySide => {
                    let cw = vp.0 / n as f32;
                    for i in 0..n {
                        let mut c = if i == a { LIME } else { INACTIVE };
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

        if self.show_ui {
            self.build_hud(&mut items, vp);
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
        for (hx, hy, vx, vy) in [
            (inset, inset, inset, inset),
            (w - inset - arm, inset, w - inset - t, inset),
            (inset, h - inset - t, inset, h - inset - arm),
            (w - inset - arm, h - inset - t, w - inset - t, h - inset - arm),
        ] {
            items.push(Item::Rect(RectItem::new(
                RectPx { x: hx, y: hy, w: arm, h: t },
                LIME,
            )));
            items.push(Item::Rect(RectItem::new(
                RectPx { x: vx, y: vy, w: t, h: arm },
                LIME,
            )));
        }

        // ---- centre A|B toggle ----
        let (seg_w, seg_h, seg_gap, pill_pad) = (42.0, 22.0, 3.0, 4.0);
        let pill_w = n as f32 * seg_w + (n as f32 - 1.0) * seg_gap + pill_pad * 2.0;
        let pill = RectPx {
            x: (w - pill_w) / 2.0,
            y: 20.0,
            w: pill_w,
            h: seg_h + pill_pad * 2.0,
        };
        items.push(Item::Rect(RectItem {
            radius: 9.0,
            border_w: 1.0,
            border_color: LIME_EDGE,
            ..RectItem::new(pill, PILL_BG)
        }));
        for i in 0..n {
            let sx = pill.x + pill_pad + i as f32 * (seg_w + seg_gap);
            let on = i == a;
            if on {
                items.push(Item::Rect(RectItem {
                    radius: 6.0,
                    ..RectItem::new(
                        RectPx { x: sx, y: pill.y + pill_pad, w: seg_w, h: seg_h },
                        LIME,
                    )
                }));
            }
            items.push(Item::Text(TextItem {
                align: Align::Center,
                valign: VAlign::Middle,
                ..TextItem::new(
                    sx + seg_w / 2.0,
                    pill.y + pill.h / 2.0,
                    12.0,
                    if on { FRAME_INK } else { SEG_OFF },
                    ((b'A' + i as u8) as char).to_string(),
                )
            }));
        }

        // ---- top-left info block ----
        let mut y = 22.0;
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
                if on { LIME } else { RULE_OFF },
            )));
            let letter_c = if on { LIME } else { INACTIVE };
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
                        LIME,
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
            fade(LIME_EDGE),
        )));

        let cy = bar.y + 13.0 + 16.0;
        // Prev / play-pause / next — the middle one on a lime disc.
        // The design's ⏮/⏸/⏭ are absent from the system mono fonts (they
        // render as nothing), so the triangles come from the geometric
        // block, which every candidate font carries, and pause is drawn
        // from two rects.
        let prev = self.btn_prev();
        let next = self.btn_next();
        let disc = self.btn_play();
        items.push(Item::Text(TextItem {
            align: Align::Center,
            valign: VAlign::Middle,
            ..TextItem::new(prev.x + prev.w / 2.0, cy, 11.0, fade(GLYPH), "◀◀")
        }));
        items.push(Item::Rect(RectItem {
            radius: disc.w / 2.0,
            ..RectItem::new(disc, fade(LIME))
        }));
        if self.playing {
            for dx in [-4.5, 1.5] {
                items.push(Item::Rect(RectItem {
                    radius: 1.0,
                    ..RectItem::new(
                        RectPx { x: disc.x + 16.0 + dx, y: cy - 6.0, w: 3.0, h: 12.0 },
                        fade(FRAME_INK),
                    )
                }));
            }
        } else {
            items.push(Item::Text(TextItem {
                align: Align::Center,
                valign: VAlign::Middle,
                ..TextItem::new(disc.x + 17.0, cy, 13.0, fade(FRAME_INK), "▶")
            }));
        }
        items.push(Item::Text(TextItem {
            align: Align::Center,
            valign: VAlign::Middle,
            ..TextItem::new(next.x + next.w / 2.0, cy, 11.0, fade(GLYPH), "▶▶")
        }));

        // Seek bar: track, lime fill to the playhead, white knob.
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
                ..RectItem::new(RectPx { w: seek.w * frac, ..seek }, fade(LIME))
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
        let extra = match self.mode {
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
            (format!("[{}] {}{}", self.mode.key(), self.mode.name(), extra), fade(LIME)),
        ] {
            items.push(Item::Text(TextItem {
                align: Align::Right,
                valign: VAlign::Middle,
                ..TextItem::new(sx, cy, 11.0, col, txt.clone())
            }));
            // Right-to-left walk; monospace so a per-char step is exact.
            sx -= txt.chars().count() as f32 * 11.0 * MONO_ADV + 8.0;
        }

        // ---- keycap row ----
        let ky = bar.y + 13.0 + 32.0 + 12.0 + 8.0;
        let mut kx = 22.0;
        for (cap, label, hot) in [
            ("ENTER", "flip A/B", true),
            ("SPACE", "play", false),
            ("< >", "frame-step", false),
            ("[ ]", "speed", false),
            ("1-6", "view mode", false),
            ("TAB", "info", false),
            ("F", "fullscreen", false),
        ] {
            let (fg, chip, shadow) = if hot {
                (FRAME_INK, LIME, LIME_SHADOW)
            } else {
                (KEYCAP_FG, KEYCAP_BG, KEYCAP_SHADOW)
            };
            items.push(Item::Text(TextItem {
                valign: VAlign::Middle,
                bg: Some(TextBg {
                    radius: 5.0,
                    pad_x: 9.0,
                    pad_y: 4.0,
                    shadow: fade(shadow),
                    shadow_dy: 2.0,
                    ..TextBg::new(fade(chip))
                }),
                ..TextItem::new(kx, ky, 11.0, fade(fg), cap)
            }));
            kx += cap.chars().count() as f32 * 11.0 * MONO_ADV + 18.0 + 7.0;
            items.push(Item::Text(TextItem {
                valign: VAlign::Middle,
                ..TextItem::new(kx, ky, 10.5, fade(LABEL), label)
            }));
            kx += label.chars().count() as f32 * 10.5 * MONO_ADV + 18.0;
        }
    }

    /// The 2b launch window: corner brackets, wordmark, two drop targets,
    /// a terminal hint and the keycap legend. Drawn from flat rects and
    /// text only (no video items), so it renders with zero streams loaded.
    fn launch_frame(&self, vp: (f32, f32)) -> FrameDesc {
        let (w, h) = vp;
        let mut items: Vec<Item> = Vec::new();
        // Only used to step BETWEEN runs (the renderer measures and
        // centres each run itself).
        let adv = |px: f32, n: usize| MONO_ADV * px * n as f32;

        // ---- corner brackets ----
        let inset = 22.0;
        let arm = 26.0;
        let t = 2.0;
        // (horizontal-arm x/y, vertical-arm x/y) for each corner.
        let corners = [
            (inset, inset, inset, inset),
            (w - inset - arm, inset, w - inset - t, inset),
            (inset, h - inset - t, inset, h - inset - arm),
            (w - inset - arm, h - inset - t, w - inset - t, h - inset - arm),
        ];
        for (hx, hy, vx, vy) in corners {
            items.push(Item::Rect(RectItem::new(
                RectPx { x: hx, y: hy, w: arm, h: t },
                LIME_DIM,
            )));
            items.push(Item::Rect(RectItem::new(
                RectPx { x: vx, y: vy, w: t, h: arm },
                LIME_DIM,
            )));
        }

        // ---- wordmark ----
        items.push(Item::Text(TextItem {
            align: Align::Center,
            tracking: 11.0,
            ..TextItem::new(w / 2.0, h * 0.13, 22.0, LIME, "ABNER")
        }));
        items.push(Item::Text(TextItem {
            align: Align::Center,
            tracking: 3.4,
            ..TextItem::new(w / 2.0, h * 0.13 + 34.0, 12.0, DIM, "A / B VIDEO COMPARE")
        }));

        // ---- two drop zones ----
        let zw = 320.0;
        let zh = 190.0;
        let gap = 24.0;
        let zx0 = (w - (zw * 2.0 + gap)) / 2.0;
        let zy = (h - zh) / 2.0 - 6.0;
        let zones: [(f32, [f32; 4], [f32; 4], [f32; 4], &str, &str, &str); 2] = [
            (
                zx0,
                LIME,
                [0.651, 0.886, 0.180, 0.05],
                [0.651, 0.886, 0.180, 0.55],
                "A",
                "drop the reference clip",
                "mp4 · mov · mkv · prores",
            ),
            (
                zx0 + zw + gap,
                TEXT,
                [1.0, 1.0, 1.0, 0.02],
                [1.0, 1.0, 1.0, 0.22],
                "B",
                "drop the encode to test",
                "or a third, fourth clip",
            ),
        ];
        for (zx, letter_col, fill, border, letter, label, sublabel) in zones {
            items.push(Item::Rect(RectItem {
                radius: 12.0,
                border_w: 1.5,
                border_color: border,
                ..RectItem::new(RectPx { x: zx, y: zy, w: zw, h: zh }, fill)
            }));
            let cx = zx + zw / 2.0;
            items.push(Item::Text(TextItem {
                align: Align::Center,
                ..TextItem::new(cx, zy + 34.0, 46.0, letter_col, letter)
            }));
            items.push(Item::Text(TextItem {
                align: Align::Center,
                ..TextItem::new(cx, zy + 108.0, 13.0, TEXT, label)
            }));
            items.push(Item::Text(TextItem {
                align: Align::Center,
                ..TextItem::new(cx, zy + 134.0, 11.0, DIM, sublabel)
            }));
        }

        // ---- terminal hint (dim · lime command · dim), centered as a group ----
        let hpx = 12.0;
        let seg: [(&str, [f32; 4]); 3] = [
            ("or run  ", DIM),
            ("abner reference.mp4 encode.mp4", LIME),
            ("  in the terminal", DIM),
        ];
        let hint_w: f32 = seg.iter().map(|(s, _)| adv(hpx, s.chars().count())).sum();
        let mut hx = (w - hint_w) / 2.0;
        let hy = h - 84.0;
        for (s, c) in seg {
            items.push(Item::Text(TextItem::new(hx, hy, hpx, c, s)));
            hx += adv(hpx, s.chars().count());
        }

        // ---- keycap legend ----
        let legend: [(&str, &str, bool); 4] = [
            ("ENTER", "flip A/B", true),
            ("SPACE", "play", false),
            ("1-6", "view mode", false),
            ("F", "fullscreen", false),
        ];
        let kpx = 11.0;
        let chip_pad = 18.0;
        let cap_gap = 7.0;
        let entry_gap = 20.0;
        let entry_w = |cap: &str, label: &str| {
            adv(kpx, cap.chars().count()) + chip_pad + cap_gap + adv(kpx, label.chars().count())
        };
        let legend_w: f32 = legend.iter().map(|e| entry_w(e.0, e.1)).sum::<f32>()
            + entry_gap * (legend.len() - 1) as f32;
        let mut lx = (w - legend_w) / 2.0;
        let ly = h - 44.0;
        for (cap, label, hot) in legend {
            let (fg, chip, shadow) = if hot {
                (DARK, LIME, LIME_SHADOW)
            } else {
                (KEYCAP_FG, KEYCAP_BG, KEYCAP_SHADOW)
            };
            items.push(Item::Text(TextItem {
                bg: Some(TextBg {
                    radius: 5.0,
                    pad_x: 9.0,
                    pad_y: 4.0,
                    shadow,
                    shadow_dy: 2.0,
                    ..TextBg::new(chip)
                }),
                ..TextItem::new(lx + 9.0, ly, kpx, fg, cap)
            }));
            let capw = adv(kpx, cap.chars().count()) + chip_pad;
            let labx = lx + capw + cap_gap;
            items.push(Item::Text(TextItem::new(labx, ly, kpx, DIM, label)));
            lx = labx + adv(kpx, label.chars().count()) + entry_gap;
        }

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

    fn mk_app(clip: &PathBuf, n: usize) -> App {
        let videos = (0..n)
            .map(|_| {
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
            })
            .collect();
        App::new(videos, Mode::Overlay)
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
/// Lime signal accent (#a6e22e) — the one hot colour in the design.
const LIME: [f32; 4] = [0.651, 0.886, 0.180, 1.0];
const LIME_DIM: [f32; 4] = [0.651, 0.886, 0.180, 0.5];
/// Hairlines and pill outlines drawn in the accent, well under full.
const LIME_EDGE: [f32; 4] = [0.651, 0.886, 0.180, 0.28];
const LIME_SHADOW: [f32; 4] = [0.451, 0.620, 0.098, 0.95];
/// Frame background / ink on lime (#050506).
const FRAME_BG: [f32; 3] = [0.0196, 0.0196, 0.0235];
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
const KEYCAP_FG: [f32; 4] = [0.910, 0.910, 0.918, 1.0];
const KEYCAP_BG: [f32; 4] = [0.149, 0.149, 0.173, 1.0];
const KEYCAP_SHADOW: [f32; 4] = [0.0, 0.0, 0.0, 0.6];

// Launch window (2b) palette.
const DARK: [f32; 4] = [0.02, 0.02, 0.024, 1.0];
const LAUNCH_BG: [f32; 3] = [0.027, 0.027, 0.035];

/// Info block width, and how many monospace chars fit inside it.
const INFO_W: f32 = 430.0;
const INFO_CH: usize = ((INFO_W - 16.0) / (10.5 * MONO_ADV)) as usize;

/// Transport strip: 13px pad + 32px controls + 12px gap + keycaps + 14px.
const TRANSPORT_H: f32 = 92.0;
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
