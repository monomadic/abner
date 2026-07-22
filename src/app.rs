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
use crate::render::{FrameDesc, Item, RectPx, Upload, VideoMode};

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
        self.drag = Some((x, y));
    }

    pub fn mouse_up(&mut self) {
        self.drag = None;
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

        if self.show_ui {
            self.build_ui(&mut items, vp);
        }
        // Big letter badges: always while the UI is on, flash-after-Enter
        // while it's off. A hugs the left edge, B the right (middle
        // videos of a >2 set interpolate across).
        let badge_alpha = if self.show_ui { 1.0 } else { (self.badge_flash / 0.4).min(1.0) };
        if badge_alpha > 0.0 {
            let push_letter =
                |items: &mut Vec<Item>, idx: usize, px: f32, active: bool, alpha: f32| {
                    let letter = (b'A' + idx as u8) as char;
                    // Monospace advance ≈ 0.62 em; enough for anchoring.
                    let w_est = px * 0.62;
                    let margin = 24.0;
                    let f = if n > 1 { idx as f32 / (n - 1) as f32 } else { 0.0 };
                    let x = margin + f * (vp.0 - w_est - 2.0 * margin);
                    let mut c = if active { ACTIVE } else { DIM };
                    c[3] *= alpha;
                    let mut bgc = BG;
                    bgc[3] *= alpha;
                    items.push(Item::Text {
                        x,
                        y: (vp.1 - px * 1.2) / 2.0,
                        px,
                        color: c,
                        text: letter.to_string(),
                        bg: Some(bgc),
                    });
                };
            match self.mode {
                // The flip view: one huge letter on that video's side.
                Mode::Overlay => push_letter(&mut items, a, 110.0, true, badge_alpha),
                // Comparing a pair: both letters, active highlighted.
                Mode::Delta | Mode::Split | Mode::Checker | Mode::Blend => {
                    push_letter(&mut items, a, 96.0, true, badge_alpha);
                    push_letter(&mut items, b, 96.0, false, badge_alpha);
                }
                // One label per cell, sitting over its own video.
                Mode::SideBySide => {
                    let cw = vp.0 / n as f32;
                    for i in 0..n {
                        let mut c = if i == a { ACTIVE } else { DIM };
                        c[3] *= badge_alpha;
                        let mut bgc = BG;
                        bgc[3] *= badge_alpha;
                        items.push(Item::Text {
                            x: i as f32 * cw + 16.0,
                            y: vp.1 - 76.0,
                            px: 48.0,
                            color: c,
                            text: ((b'A' + i as u8) as char).to_string(),
                            bg: Some(bgc),
                        });
                    }
                }
            }
        }

        let animating = self.playing || self.badge_flash > 0.0 || !self.started;
        FrameDesc {
            clear: [0.02, 0.02, 0.025],
            uploads,
            items,
            animating,
            redraw_at: if animating { None } else { Some(Instant::now() + std::time::Duration::from_millis(100)) },
        }
    }

    fn build_ui(&mut self, items: &mut Vec<Item>, vp: (f32, f32)) {
        let mut y = 14.0;
        let a = self.active;
        for (i, v) in self.videos.iter().enumerate() {
            let letter = (b'A' + i as u8) as char;
            let marker = if i == a { "▶" } else { " " };
            let name = v
                .info
                .path
                .file_name()
                .map(|f| f.to_string_lossy().into_owned())
                .unwrap_or_default();
            let color = if i == a { ACTIVE } else { TEXT };
            items.push(Item::Text {
                x: 14.0,
                y,
                px: 14.0,
                color,
                text: format!("{marker} {letter}  {name}"),
                bg: Some(BG),
            });
            y += 21.0;
            let failed = v.player.failed();
            let br = v
                .info
                .bit_rate
                .map(|b| format!("{:.2} Mb/s", b as f64 / 1e6))
                .unwrap_or_else(|| "? Mb/s".into());
            let detail = if failed {
                "   DECODE FAILED".to_string()
            } else {
                format!(
                    "   {}×{}  {:.3} fps  {} {}  {}  {}  {}",
                    v.info.width,
                    v.info.height,
                    v.info.fps,
                    v.info.codec,
                    v.info.pix_fmt,
                    br,
                    fmt_size(v.info.file_size),
                    fmt_time(v.info.duration),
                )
            };
            items.push(Item::Text {
                x: 14.0,
                y,
                px: 12.0,
                color: if failed { ERR } else { DIM },
                text: detail,
                bg: Some(BG),
            });
            y += 18.0;
            let dir = v
                .info
                .path
                .parent()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
            items.push(Item::Text {
                x: 14.0,
                y,
                px: 12.0,
                color: DIM,
                text: format!("   {dir}"),
                bg: Some(BG),
            });
            y += 26.0;
        }

        // Status line.
        let frame = (self.t * self.fps).round() as i64;
        let zoom = if self.zoom > 1.001 {
            format!("{:.1}×", self.zoom)
        } else {
            "fit".to_string()
        };
        let extra = match self.mode {
            Mode::Delta => format!("  gain ×{:.1}", self.gain),
            Mode::Blend => format!("  blend {:.0}%", self.blend * 100.0),
            Mode::Checker => format!("  checker {:.0}px", self.checker_px),
            _ => String::new(),
        };
        let speed = if (self.speed - 1.0).abs() > 1e-3 {
            format!("   speed ×{:.2}", self.speed)
        } else {
            String::new()
        };
        let status = format!(
            "[{}] {}{}   {} / {}   frame {}   {}{}   {}",
            self.mode.key(),
            self.mode.name(),
            extra,
            fmt_time(self.t),
            if self.wrap.is_finite() { fmt_time(self.wrap) } else { "?".into() },
            frame,
            if self.playing { "playing" } else { "paused" },
            speed,
            zoom,
        );
        items.push(Item::Text {
            x: 14.0,
            y: vp.1 - 32.0,
            px: 13.0,
            color: TEXT,
            text: status,
            bg: Some(BG),
        });
    }

    /// The 2b launch window: corner brackets, wordmark, two drop targets,
    /// a terminal hint and the keycap legend. Drawn from flat rects and
    /// text only (no video items), so it renders with zero streams loaded.
    /// Widths are estimated at the monospace advance (~0.6 em) to center.
    fn launch_frame(&self, vp: (f32, f32)) -> FrameDesc {
        let (w, h) = vp;
        let mut items: Vec<Item> = Vec::new();
        let adv = |px: f32, n: usize| 0.60 * px * n as f32;

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
            items.push(Item::Rect { r: RectPx { x: hx, y: hy, w: arm, h: t }, color: LIME_DIM });
            items.push(Item::Rect { r: RectPx { x: vx, y: vy, w: t, h: arm }, color: LIME_DIM });
        }

        // ---- wordmark ----
        let word = "A B N E R";
        let wpx = 22.0;
        items.push(Item::Text {
            x: (w - adv(wpx, word.chars().count())) / 2.0,
            y: h * 0.13,
            px: wpx,
            color: LIME,
            text: word.into(),
            bg: None,
        });
        let sub = "A / B VIDEO COMPARE";
        let spx = 12.0;
        items.push(Item::Text {
            x: (w - adv(spx, sub.chars().count())) / 2.0,
            y: h * 0.13 + 34.0,
            px: spx,
            color: DIM,
            text: sub.into(),
            bg: None,
        });

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
                "mp4 / mov / mkv / prores",
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
            items.push(Item::Rect { r: RectPx { x: zx, y: zy, w: zw, h: zh }, color: fill });
            let bt = 1.5;
            items.push(Item::Rect { r: RectPx { x: zx, y: zy, w: zw, h: bt }, color: border });
            items.push(Item::Rect { r: RectPx { x: zx, y: zy + zh - bt, w: zw, h: bt }, color: border });
            items.push(Item::Rect { r: RectPx { x: zx, y: zy, w: bt, h: zh }, color: border });
            items.push(Item::Rect { r: RectPx { x: zx + zw - bt, y: zy, w: bt, h: zh }, color: border });
            let cx = zx + zw / 2.0;
            let big = 46.0;
            items.push(Item::Text {
                x: cx - adv(big, 1) / 2.0,
                y: zy + 34.0,
                px: big,
                color: letter_col,
                text: letter.into(),
                bg: None,
            });
            let lb = 13.0;
            items.push(Item::Text {
                x: cx - adv(lb, label.chars().count()) / 2.0,
                y: zy + 108.0,
                px: lb,
                color: TEXT,
                text: label.into(),
                bg: None,
            });
            let sb = 11.0;
            items.push(Item::Text {
                x: cx - adv(sb, sublabel.chars().count()) / 2.0,
                y: zy + 134.0,
                px: sb,
                color: DIM,
                text: sublabel.into(),
                bg: None,
            });
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
            items.push(Item::Text { x: hx, y: hy, px: hpx, color: c, text: s.into(), bg: None });
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
        let chip_pad = 12.0; // renderer pads text bg by 6px each side
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
            let (fg, bg) = if hot { (DARK, LIME) } else { (KEYCAP_FG, KEYCAP_BG) };
            items.push(Item::Text { x: lx + 6.0, y: ly, px: kpx, color: fg, text: cap.into(), bg: Some(bg) });
            let capw = adv(kpx, cap.chars().count()) + chip_pad;
            let labx = lx + capw + cap_gap;
            items.push(Item::Text { x: labx, y: ly, px: kpx, color: DIM, text: label.into(), bg: None });
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

const TEXT: [f32; 4] = [0.92, 0.92, 0.92, 1.0];
const DIM: [f32; 4] = [0.62, 0.62, 0.66, 1.0];
const ACTIVE: [f32; 4] = [1.0, 0.82, 0.25, 1.0];
const ERR: [f32; 4] = [1.0, 0.35, 0.3, 1.0];
const BG: [f32; 4] = [0.0, 0.0, 0.0, 0.55];

// Launch window (2b) palette.
const LIME: [f32; 4] = [0.651, 0.886, 0.180, 1.0];
const LIME_DIM: [f32; 4] = [0.651, 0.886, 0.180, 0.5];
const DARK: [f32; 4] = [0.02, 0.02, 0.024, 1.0];
const KEYCAP_FG: [f32; 4] = [0.9, 0.9, 0.91, 1.0];
const KEYCAP_BG: [f32; 4] = [0.149, 0.149, 0.172, 1.0];
const LAUNCH_BG: [f32; 3] = [0.027, 0.027, 0.035];

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
