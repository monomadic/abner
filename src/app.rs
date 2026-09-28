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
use crate::mask::{self, Corner, Crop, Mask};
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
    /// ⌘W: close the focused clip.
    Close,
}

#[derive(Debug, Clone, Copy)]
pub enum Cmd {
    Quit,
    ToggleFullscreen,
    /// The clip list changed shape outside a drop (⌘W): the runner
    /// re-syncs the GPU's per-slot textures and the window title.
    VideosChanged,
}

/// What a press on the crop marquee took hold of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CropGrab {
    /// Anywhere inside: slide the whole rect, size unchanged.
    Move,
    /// One of the white squares: drag it, opposite corner anchored.
    Corner(Corner),
}

pub struct Video {
    pub info: VideoInfo,
    pub player: Player,
    pub shown_pts: f64,
    pub delivered: bool,
    /// Waiting to adopt the first frame after an exact seek while paused.
    pub pending: bool,
    /// The frame on screen, kept only while mask mode is on: a crop export
    /// writes these pixels, and the GPU's copy can't be read back. Mask
    /// mode is paused, so this costs one copy per seek, not per frame.
    pub last_frame: Option<std::sync::Arc<Vec<u8>>>,
}

pub struct App {
    pub videos: Vec<Video>,
    active: usize,
    source_scroll: usize,
    masks: Vec<Option<Mask>>,
    mask_mode: bool,
    brush_diameter: f32,
    painting: bool,
    stroke_last: Option<(f32, f32)>,
    cursor_inside: bool,
    mask_status: String,
    mask_save: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    /// The ProRes crop export (`E`), an ffmpeg child on a worker. Separate
    /// from `mask_save` because it runs for as long as the clip takes to
    /// encode, and a PNG save shouldn't queue behind it.
    crop_export: Option<std::sync::mpsc::Receiver<Result<String, String>>>,
    /// The crop marquee (`C`), in the ACTIVE video's image pixels. While
    /// it is up the pointer moves/resizes it instead of painting, and `S`
    /// exports what it holds instead of the whole frame.
    crop: Option<Crop>,
    /// What the pointer grabbed, and where inside it (image px), so the
    /// marquee doesn't jump to the cursor on the first move.
    crop_drag: Option<(CropGrab, (f32, f32))>,
    /// Index into `ASPECTS` (`A` cycles): the marquee's locked ratio, 0 for
    /// free. A tool setting, so it outlives hiding the marquee.
    aspect: usize,
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
    /// Seconds per ← / → (config `playback.seek_step`).
    seek_step: f64,
    show_ui: bool,
    /// Delta amplification.
    gain: f32,
    blend: f32,
    checker_px: f32,
    /// Seconds left on the small clip-number flash (shown after Enter
    /// when the UI is hidden — switchblade's skip-bar-flash pattern).
    badge_flash: f32,
    fullscreen: bool,
    cursor: (f32, f32),
    /// Last pointer position while a drag-pan is held.
    drag: Option<(f32, f32)>,
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
    /// The launch plate's pixel size and the v of its measured horizon,
    /// handed over by the renderer the same way (`Gpu::plate_horizon`).
    plate_size: (f32, f32),
    plate_horizon: f32,
    /// The launch backdrop video (none when the asset is missing or failed
    /// to decode: the still plate stays), and its decoder while
    /// the launch window is up.
    backdrop_src: Option<VideoInfo>,
    backdrop: Option<Backdrop>,
    /// The window is occluded: the backdrop's clock stops, so nothing
    /// drains its queue and backpressure parks the decoder.
    hidden: bool,
    cmds: Vec<Cmd>,
}

/// The launch window's moving floor: a decoder of its own and a clock of
/// its own that wraps at the clip's length. The clip is authored as a
/// seamless loop (`assets/banner/background-02-loop.mp4`), so the wrap is
/// just an exact seek to 0. It only runs while there are no clips — a
/// loaded clip drops it, so it never competes with the streams.
struct Backdrop {
    player: Player,
    t: f64,
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
    pub fn new(videos: Vec<Video>, cfg: &crate::config::Config) -> Self {
        let fps = Self::fps_of(&videos);
        let wrap = Self::wrap_of(&videos);
        Self {
            masks: (0..videos.len()).map(|_| None).collect(),
            mask_mode: false,
            brush_diameter: cfg.mask.brush_size,
            painting: false,
            stroke_last: None,
            cursor_inside: false,
            mask_status: String::new(),
            mask_save: None,
            crop_export: None,
            crop: None,
            crop_drag: None,
            aspect: 0,
            videos,
            active: 0,
            source_scroll: 0,
            t: 0.0,
            playing: !cfg.playback.start_paused,
            started: false,
            mode: cfg.playback.view,
            zoom: 1.0,
            center: (0.5, 0.5),
            speed: 1.0,
            seek_step: cfg.playback.seek_step,
            show_ui: true,
            gain: cfg.compare.delta_gain,
            blend: cfg.compare.blend,
            checker_px: cfg.compare.checker_size,
            badge_flash: 0.0,
            fullscreen: false,
            cursor: (0.0, 0.0),
            drag: None,
            scrubbing: false,
            vp: (1280.0, 800.0),
            fps,
            wrap,
            drag_hover: false,
            logo_aspect: 3.0,
            plate_size: (1448.0, 1086.0),
            plate_horizon: 0.616,
            backdrop_src: None,
            backdrop: None,
            hidden: false,
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

    /// Show the marquee at the full frame, or hide it again. Hiding drops
    /// the rect: `S` goes back to exporting the whole mask.
    fn toggle_crop(&mut self) {
        self.crop_drag = None;
        self.painting = false;
        self.stroke_last = None;
        self.mask_status.clear();
        self.crop = match self.crop {
            Some(_) => None,
            None => Some(self.full_crop()),
        };
    }

    /// Open mask mode with the marquee up (`--crop`), optionally at an
    /// explicit image-pixel rect. The CLI route into the state, so a
    /// visual check never needs injected keystrokes.
    pub fn start_crop(&mut self, rect: Option<[f32; 4]>) {
        if !self.ready() {
            return;
        }
        if !self.mask_mode {
            self.key(Key::Char('m'));
        }
        if self.crop.is_none() {
            self.toggle_crop();
        }
        if let Some([x, y, w, h]) = rect {
            let mask = self.masks[self.active].as_ref().unwrap();
            self.crop = Some(Crop { x, y, w, h }.clamped(mask.width, mask.height));
        }
    }

    /// The whole frame, or the largest rect of the locked ratio in it.
    fn full_crop(&self) -> Crop {
        let mask = self.masks[self.active].as_ref().unwrap();
        let full = Crop::full(mask.width, mask.height);
        match ASPECTS[self.aspect].1 {
            Some(r) => full.with_aspect(r, mask.width, mask.height),
            None => full,
        }
    }

    /// Step the ratio preset and reshape the marquee to it (free leaves the
    /// rect where it is, just unlocked).
    fn cycle_aspect(&mut self, step: isize) {
        let n = ASPECTS.len() as isize;
        self.aspect = (self.aspect as isize + step).rem_euclid(n) as usize;
        let mask = self.masks[self.active].as_ref().unwrap();
        if let (Some(c), Some(r)) = (self.crop, ASPECTS[self.aspect].1) {
            self.crop = Some(c.with_aspect(r, mask.width, mask.height));
        }
        self.crop_drag = None;
        self.mask_status.clear();
    }

    /// Pointer position in the active video's image pixels — the mask's
    /// grid, and the crop's. The one zoom transform, never a second one.
    fn image_point(&self, x: f32, y: f32) -> (f32, f32) {
        let r = self.content_rect(self.active);
        let mask = self.masks[self.active].as_ref().unwrap();
        ((x - r.x) / r.w * mask.width as f32, (y - r.y) / r.h * mask.height as f32)
    }

    /// The marquee on screen, through the same transform the video uses.
    fn crop_rect(&self, c: Crop) -> RectPx {
        let r = self.content_rect(self.active);
        let mask = self.masks[self.active].as_ref().unwrap();
        let (sx, sy) = (r.w / mask.width as f32, r.h / mask.height as f32);
        RectPx { x: r.x + c.x * sx, y: r.y + c.y * sy, w: c.w * sx, h: c.h * sy }
    }

    /// Take hold of the marquee: a white square if the press is on one,
    /// otherwise the body. A press outside it grabs nothing (the dimmed
    /// area isn't part of the export, so there is nothing to do there).
    fn crop_grab(&mut self, x: f32, y: f32) {
        let Some(c) = self.crop else { return };
        let s = self.crop_rect(c);
        let point = self.image_point(x, y);
        for (corner, sx, sy) in [
            (Corner::Nw, s.x, s.y),
            (Corner::Ne, s.x + s.w, s.y),
            (Corner::Sw, s.x, s.y + s.h),
            (Corner::Se, s.x + s.w, s.y + s.h),
        ] {
            if (x - sx).abs() <= CROP_GRAB && (y - sy).abs() <= CROP_GRAB {
                let (hx, hy) = corner.of(c);
                self.crop_drag = Some((CropGrab::Corner(corner), (point.0 - hx, point.1 - hy)));
                return;
            }
        }
        if c.contains(point.0, point.1) {
            self.crop_drag = Some((CropGrab::Move, (point.0 - c.x, point.1 - c.y)));
        }
    }

    fn crop_drag_to(&mut self, x: f32, y: f32) {
        let (Some(c), Some((grab, (ox, oy)))) = (self.crop, self.crop_drag) else { return };
        let mask = self.masks[self.active].as_ref().unwrap();
        let (w, h) = (mask.width, mask.height);
        let (px, py) = self.image_point(x, y);
        self.crop = Some(match grab {
            CropGrab::Move => c.moved_to(px - ox, py - oy, w, h),
            CropGrab::Corner(corner) => match ASPECTS[self.aspect].1 {
                Some(r) => c.with_corner_locked(corner, px - ox, py - oy, r, w, h),
                None => c.with_corner(corner, px - ox, py - oy, w, h),
            },
        });
        self.mask_status.clear();
    }

    /// The mask uses exactly the canvas video transform. The workspace shell,
    /// the traffic-light strip and the letterbox are not paint targets; leaving
    /// them breaks stroke continuity.
    pub fn brush_cursor_visible(&self) -> bool {
        // The marquee owns the pointer while it is up — the brush would be
        // painting into a region the crop is about to throw away.
        if !self.mask_mode || !self.cursor_inside || !self.ready() || self.crop.is_some() {
            return false;
        }
        let r = self.content_rect(self.active);
        let (x, y) = self.cursor;
        x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
            && contains(self.workspace().canvas, x, y)
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

    /// Export the mask — and, with the marquee up, the crop of it plus the
    /// video pixels underneath, cut to the very same rectangle so the two
    /// files line up pixel for pixel.
    fn save_mask(&mut self) {
        if self.mask_save.is_some() { return; }
        let mask = self.masks[self.active].as_ref().unwrap();
        let video = &self.videos[self.active];
        let path = mask::output_path(&video.info.path);
        let (width, height, pixels, frame) = match self.crop {
            None => (mask.width, mask.height, mask.pixels.clone(), None),
            Some(c) => {
                let rect = c.pixels(mask.width, mask.height);
                // The frame is whatever mask mode last put on screen; it is
                // missing only if no frame has arrived yet, and then the
                // mask alone still saves.
                let frame = video.last_frame.as_ref().map(|f| {
                    (
                        mask::crop_output_path(&video.info.path),
                        mask::crop_pixels(f, mask.width, 4, rect),
                    )
                });
                (rect.2, rect.3, std::sync::Arc::new(
                    mask::crop_pixels(&mask.pixels, mask.width, 1, rect),
                ), frame)
            }
        };
        let (sender, receiver) = std::sync::mpsc::channel();
        self.mask_save = Some(receiver);
        self.mask_status = "Saving…".into();
        std::thread::spawn(move || {
            let result = (|| -> anyhow::Result<String> {
                mask::save(&path, width, height, &pixels)?;
                let mut saved = name_of(&path);
                if let Some((crop_path, crop_pixels)) = frame {
                    mask::save_rgba(&crop_path, width, height, &crop_pixels)?;
                    saved = format!("{saved} + {}", name_of(&crop_path));
                }
                Ok(format!("{saved} ({width}×{height})"))
            })()
            .map_err(|e| e.to_string());
            let _ = sender.send(result);
        });
    }

    /// Re-encode the active clip, whole, cut to the marquee, as ProRes 422
    /// Proxy beside the source (`clip.crop.mov`). The frame-accurate twin
    /// of `S`'s still: same rectangle, every frame.
    fn export_crop(&mut self) {
        let Some(c) = self.crop else { return };
        if self.crop_export.is_some() { return; }
        let mask = self.masks[self.active].as_ref().unwrap();
        let rect = c.pixels(mask.width, mask.height);
        let (_, _, w, h) = mask::even_rect(rect);
        let source = self.videos[self.active].info.path.clone();
        let dest = mask::crop_video_path(&source);
        let (sender, receiver) = std::sync::mpsc::channel();
        self.crop_export = Some(receiver);
        self.mask_status = format!("Exporting {} ({w}×{h})…", name_of(&dest));
        std::thread::spawn(move || {
            let result = mask::export_prores(&source, &dest, rect)
                .map(|()| format!("{} ({w}×{h} ProRes Proxy)", name_of(&dest)))
                .map_err(|e| e.to_string());
            let _ = sender.send(result);
        });
    }

    fn build_mask_layer(&self, items: &mut Vec<Item>) {
        let mask = self.masks[self.active].as_ref().unwrap();
        let r = self.content_rect(self.active);
        // Crop mode shows the footage itself: no red/blue mask tint, just
        // the dimming outside the marquee. The mask is still there (and
        // still exported with `S`), only not drawn.
        if let Some(c) = self.crop {
            self.build_crop_layer(items, c);
        } else {
            items.push(Item::Mask { r, id: mask.id, revision: mask.revision,
                width: mask.width, height: mask.height, pixels: mask.pixels.clone() });
        }
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

    /// The marquee: everything outside it dimmed away, a dashed border and
    /// a white square on each corner. The dashes are little rects — the
    /// renderer has no line primitive — and every piece is drawn twice,
    /// dark underlay then white, so the box holds against bright footage
    /// the way the brush cursor does.
    fn build_crop_layer(&self, items: &mut Vec<Item>, c: Crop) {
        let r = self.crop_rect(c);
        let (vw, vh) = self.vp;
        let (x1, y1) = ((r.x + r.w).clamp(0.0, vw), (r.y + r.h).clamp(0.0, vh));
        let (x0, y0) = (r.x.clamp(0.0, vw), r.y.clamp(0.0, vh));
        for dim in [
            RectPx { x: 0.0, y: 0.0, w: vw, h: y0 },
            RectPx { x: 0.0, y: y1, w: vw, h: vh - y1 },
            RectPx { x: 0.0, y: y0, w: x0, h: y1 - y0 },
            RectPx { x: x1, y: y0, w: vw - x1, h: y1 - y0 },
        ] {
            if dim.w > 0.0 && dim.h > 0.0 {
                items.push(Item::Rect(RectItem::new(dim, CROP_DIM)));
            }
        }
        let mut edge = |x: f32, y: f32, run: f32, horizontal: bool| {
            let mut at = 0.0;
            while at < run {
                let len = CROP_DASH.min(run - at);
                let seg = if horizontal {
                    RectPx { x: x + at, y, w: len, h: CROP_LINE_W }
                } else {
                    RectPx { x, y: y + at, w: CROP_LINE_W, h: len }
                };
                for (grow, color) in [(1.0, CROP_INK), (0.0, CROP_LINE)] {
                    items.push(Item::Rect(RectItem::new(
                        RectPx {
                            x: seg.x - grow,
                            y: seg.y - grow,
                            w: seg.w + grow * 2.0,
                            h: seg.h + grow * 2.0,
                        },
                        color,
                    )));
                }
                at += CROP_DASH + CROP_GAP;
            }
        };
        let half = CROP_LINE_W * 0.5;
        edge(r.x, r.y - half, r.w, true);
        edge(r.x, r.y + r.h - half, r.w, true);
        edge(r.x - half, r.y, r.h, false);
        edge(r.x + r.w - half, r.y, r.h, false);
        for (hx, hy) in
            [(r.x, r.y), (r.x + r.w, r.y), (r.x, r.y + r.h), (r.x + r.w, r.y + r.h)]
        {
            for (size, color) in [(CROP_HANDLE + 2.0, CROP_INK), (CROP_HANDLE, CROP_LINE)] {
                items.push(Item::Rect(RectItem::new(
                    RectPx { x: hx - size * 0.5, y: hy - size * 0.5, w: size, h: size },
                    color,
                )));
            }
        }
    }

    pub fn set_logo_aspect(&mut self, aspect: f32) {
        self.logo_aspect = aspect;
    }

    /// The plate's proportions and horizon, measured by the renderer.
    pub fn set_plate(&mut self, size: (f32, f32), horizon: f32) {
        self.plate_size = size;
        self.plate_horizon = horizon;
    }

    /// The video the launch window plays as its floor (probed by main).
    pub fn set_backdrop(&mut self, info: VideoInfo) {
        self.backdrop_src = Some(info);
    }

    /// Occluded windows don't decode the backdrop (see `hidden`).
    pub fn set_hidden(&mut self, hidden: bool) {
        self.hidden = hidden;
    }

    /// A backdrop frame goes back to its decoder's pool.
    pub fn recycle_backdrop(&mut self, buf: Vec<u8>) {
        if let Some(b) = &self.backdrop {
            b.player.recycle(buf);
        }
    }

    /// Advance the backdrop's clock and take the frame now due, spawning
    /// the decoder on the launch window's first frame.
    fn tick_backdrop(&mut self, dt: f32) -> Option<Upload> {
        let src = self.backdrop_src.as_ref()?;
        // Nobody can see the floor: hold the clock and take nothing. The
        // full queue stalls the reader, so a hidden launch window costs no
        // decode, and it resumes where it left off.
        if self.hidden {
            return None;
        }
        if self.backdrop.is_none() {
            let player = Player::spawn(
                &src.path,
                src.width,
                src.height,
                crate::probe::vt_accel(&src.codec),
                src.rotation,
            )?;
            self.backdrop = Some(Backdrop { player, t: 0.0 });
        }
        let b = self.backdrop.as_mut()?;
        if b.player.failed() {
            log::warn!("launch backdrop failed to decode; keeping the still plate");
            self.backdrop = None;
            self.backdrop_src = None;
            return None;
        }
        b.t += dt as f64;
        // Wrap half a frame early so the last frame isn't held while the
        // seek lands.
        if src.duration > 0.0 && b.t >= src.duration - 0.5 / src.fps.max(1.0) {
            b.player.seek(0.0, true);
            b.t = 0.0;
        }
        let (_, buf) = b.player.take_upto(b.t + 1e-6)?;
        Some(Upload { idx: 0, w: b.player.w, h: b.player.h, buf })
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
        self.drag = None;
        self.scrubbing = false;
        self.drag_hover = false;
    }

    /// Drop the focused clip (⌘W). The survivors shift down a slot, and
    /// the GPU's textures are indexed by slot, so every survivor is
    /// exact-seeked to the current `t`: each re-delivers the frame for
    /// this moment into its new slot (a paused one too, via `pending`)
    /// rather than showing its neighbour's last frame. Closing the last
    /// clip returns to the launch window.
    fn close_active(&mut self) {
        if self.videos.is_empty() {
            return;
        }
        let idx = self.active.min(self.videos.len() - 1);
        self.videos.remove(idx);
        self.masks.remove(idx);
        self.cmds.push(Cmd::VideosChanged);
        self.mouse_up();
        self.stroke_last = None;
        if self.videos.is_empty() {
            self.mask_mode = false;
            self.mask_status.clear();
            self.active = 0;
            self.t = 0.0;
            self.zoom = 1.0;
            self.center = (0.5, 0.5);
            return;
        }
        self.active = idx.min(self.videos.len() - 1);
        self.badge_flash = 1.2;
        self.fps = Self::fps_of(&self.videos);
        self.wrap = Self::wrap_of(&self.videos);
        let max = if self.wrap.is_finite() { (self.wrap - 0.05).max(0.0) } else { f64::MAX };
        // `started` stays as it is: an exact seek lands on the first frame
        // AT OR AFTER `t`, so gating the clock on delivery would wait
        // forever for a pts it never advances to (as with `seek_by`).
        self.seek_all(self.t.clamp(0.0, max), true);
        if self.mask_mode {
            self.ensure_mask();
            self.mask_status.clear();
        }
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
                // Nothing left to close: ⌘W on the empty window closes
                // it, which for a one-window app is quitting.
                Key::Close => self.cmds.push(Cmd::Quit),
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
        if k == Key::Close {
            self.close_active();
            return;
        }
        if k == Key::Char('m') || k == Key::Char('M') {
            self.mask_mode = !self.mask_mode;
            self.mouse_up();
            if self.mask_mode {
                self.playing = false;
                self.ensure_mask();
                // Re-deliver the frame on screen so a crop export has the
                // pixels: the ones already shown went back to the decoder.
                self.seek_all(self.t, true);
            } else {
                self.leave_mask_mode();
            }
            return;
        }
        if !self.mask_mode && matches!(k, Key::Char('c' | 'C')) {
            self.activate(Action::Tool(2));
            return;
        }
        if self.mask_mode {
            match k {
                Key::Char(']' | '+' | '=') => { self.resize_brush(1.25); return; }
                Key::Char('[' | '-' | '_') => { self.resize_brush(0.8); return; }
                Key::Char('c' | 'C') => { self.toggle_crop(); return; }
                Key::Char('s' | 'S') => { self.save_mask(); return; }
                Key::Char('e' | 'E') if self.crop.is_some() => { self.export_crop(); return; }
                Key::Char('a') if self.crop.is_some() => { self.cycle_aspect(1); return; }
                Key::Char('A') if self.crop.is_some() => { self.cycle_aspect(-1); return; }
                Key::Escape => { self.mask_mode = false; self.leave_mask_mode(); return; }
                _ => {}
            }
            self.stroke_last = None;
        }
        match k {
            Key::Enter => self.select((self.active + 1) % self.videos.len()),
            Key::Space => self.playing = !self.playing,
            Key::Tab => { self.mouse_up(); self.show_ui = !self.show_ui; },
            Key::Left => self.seek_by(-self.seek_step),
            Key::Right => self.seek_by(self.seek_step),
            Key::Escape => {
                if self.fullscreen {
                    self.fullscreen = false;
                    self.cmds.push(Cmd::ToggleFullscreen);
                } else {
                    self.cmds.push(Cmd::Quit);
                }
            }
            Key::Backspace => self.speed = 1.0,
            Key::Close => {} // handled above
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
        let capacity = self.source_capacity();
        if idx < self.source_first() { self.source_scroll = idx; }
        else if idx >= self.source_first() + capacity { self.source_scroll = idx + 1 - capacity; }
        self.badge_flash = 1.2;
        self.mouse_up();
        if self.mask_mode {
            self.ensure_mask();
            self.mask_status.clear();
            // The new video has its own dimensions; a marquee carried over
            // from the old one would mean nothing.
            if self.crop.is_some() { self.crop = Some(self.full_crop()); }
        }
    }

    pub fn cursor_moved(&mut self, x: f32, y: f32) {
        self.cursor_inside = true;
        if self.scrubbing {
            self.scrub_to(x);
            self.cursor = (x, y);
            return;
        }
        if self.mask_mode {
            self.cursor = (x, y);
            if self.crop_drag.is_some() {
                self.crop_drag_to(x, y);
            } else if self.painting {
                self.paint_at_cursor();
            }
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
        if !self.ready() { return; }
        self.cursor = (x, y);
        self.cursor_inside = true;
        if self.show_ui {
            if let Some(control) = self.controls().into_iter().find(|c| contains(c.r, x, y)) {
                self.activate(control.action);
                return;
            }
            if contains(self.workspace().list, x, y) {
                let first = self.source_first();
                for idx in first..(first + self.source_capacity()).min(self.videos.len()) {
                    if contains(self.source_row(idx), x, y) { self.select(idx); break; }
                }
                return;
            }
            if contains(self.btn_prev(), x, y) { self.step(-1); return; }
            if contains(self.btn_play(), x, y) { self.playing = !self.playing; return; }
            if contains(self.btn_next(), x, y) { self.step(1); return; }
            let s = self.seek_rect(self.vp);
            if contains(RectPx { x: s.x - 6.0, y: s.y - SEEK_GRAB, w: s.w + 12.0, h: s.h + 2.0 * SEEK_GRAB }, x, y) {
                self.scrubbing = true;
                self.scrub_to(x);
                return;
            }
        }
        if !contains(self.workspace().canvas, x, y) { return; }
        if self.mask_mode {
            self.stroke_last = None;
            if self.crop.is_some() { self.crop_grab(x, y); return; }
            self.painting = self.brush_cursor_visible();
            if self.painting { self.paint_at_cursor(); }
        } else {
            self.drag = Some((x, y));
        }
    }

    pub fn mouse_up(&mut self) {
        self.painting = false;
        self.stroke_last = None;
        self.drag = None;
        self.scrubbing = false;
        self.crop_drag = None;
    }

    /// Leaving mask mode drops the marquee and the frames it was holding
    /// for export — a full RGBA copy per video.
    fn leave_mask_mode(&mut self) {
        self.mouse_up();
        self.crop = None;
        for v in &mut self.videos {
            v.last_frame = None;
        }
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
        if !self.ready() { return; }
        if self.show_ui && contains(self.workspace().list, self.cursor.0, self.cursor.1) {
            let first = self.source_first();
            self.source_scroll = if dy < 0.0 { (first + 1).min(self.videos.len().saturating_sub(self.source_capacity())) }
                else if dy > 0.0 { first.saturating_sub(1) } else { first };
            return;
        }
        if !contains(self.workspace().canvas, self.cursor.0, self.cursor.1) { return; }
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
        if !self.ready() || !contains(self.workspace().canvas, self.cursor.0, self.cursor.1) { return; }
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
    /// hovered cell in side-by-side, the active video's canvas fit
    /// otherwise.
    fn video_cell(&self, idx: usize) -> RectPx {
        let mut cell = self.workspace().canvas;
        if self.shown_mode() == Mode::SideBySide && !self.mask_mode {
            cell.w /= self.videos.len().max(1) as f32;
            cell.x += idx as f32 * cell.w;
        }
        cell
    }

    fn gesture_base(&self, x: f32, _y: f32) -> RectPx {
        let idx = if self.shown_mode() == Mode::SideBySide && !self.mask_mode {
            let c = self.workspace().canvas;
            (((x - c.x).max(0.0) / (c.w / self.videos.len() as f32).max(1.0)) as usize).min(self.videos.len() - 1)
        } else { self.active };
        self.base_rect(idx)
    }

    fn base_rect(&self, idx: usize) -> RectPx {
        let dims = (self.videos[idx].info.width, self.videos[idx].info.height);
        Self::fit_rect(dims, inset_rect(self.video_cell(idx), if self.show_ui { 24.0 } else { 0.0 }))
    }

    fn content_rect(&self, idx: usize) -> RectPx {
        Self::zoomed(self.base_rect(idx), self.zoom, self.center)
    }

    pub fn tick(&mut self, dt: f32, vp: (f32, f32), _scale: f32) -> FrameDesc {
        if self.vp != vp { self.stroke_last = None; }
        self.vp = vp;
        for (slot, what, done) in [
            (&mut self.mask_save, "Save", "Saved"),
            (&mut self.crop_export, "Export", "Exported"),
        ] {
            let Some(receiver) = slot else { continue };
            match receiver.try_recv() {
                Ok(result) => {
                    self.mask_status = match result {
                        Ok(saved) => format!("{done} {saved}"),
                        Err(error) => { log::error!("{what} failed: {error}"); format!("{what} failed: {error}") }
                    };
                    *slot = None;
                }
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    self.mask_status = format!("{what} failed: worker stopped");
                    *slot = None;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
            }
        }
        // Nothing loaded — paint the launch window and stop.
        if !self.ready() {
            let plate = self.tick_backdrop(dt);
            let mut desc = self.launch_frame(vp);
            // The floor moves at the clip's own rate: wake for its next
            // frame instead of running the loop hot at the display's.
            if let Some(b) = &self.backdrop_src {
                desc.redraw_at =
                    Some(Instant::now() + std::time::Duration::from_secs_f64(1.0 / b.fps.max(1.0)));
            }
            desc.plate = plate;
            return desc;
        }
        // Clips are up: the backdrop's decoder has nothing left to do.
        self.backdrop = None;
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
        let capture = self.mask_mode;
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
                // The GPU's copy can't be read back, so a crop export needs
                // its own. Mask mode is paused: this runs on entry and on
                // seeks, not once a frame.
                if capture {
                    v.last_frame = Some(std::sync::Arc::new(buf.clone()));
                }
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
        let canvas = self.workspace().canvas;
        if self.show_ui { items.push(Item::Rect(RectItem::new(canvas, CANVAS_BG))); }
        items.push(Item::Clip(Some(canvas)));
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
                for i in 0..n {
                    let cell = self.video_cell(i);
                    items.push(Item::Clip(Some(cell)));
                    let dims = (self.videos[i].info.width, self.videos[i].info.height);
                    // Every cell shares the zoom/center, so panning one
                    // pans them all to the same content position.
                    let base = Self::fit_rect(dims, inset_rect(cell, if self.show_ui { 24.0 } else { 0.0 }));
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

        items.push(Item::Clip(Some(canvas)));
        if self.mask_mode { self.build_mask_layer(&mut items); }
        items.push(Item::Clip(None));
        if self.show_ui || self.badge_flash > 0.0 {
            for i in 0..if mode == Mode::SideBySide { n } else { 1 } {
                let idx = if mode == Mode::SideBySide { i } else { a };
                let cell = if mode == Mode::SideBySide { self.video_cell(idx) } else { self.workspace().canvas };
                let image = self.content_rect(idx);
                items.push(Item::Clip(Some(cell)));
                number_badge(&mut items, RectPx { x: image.x.max(cell.x) + 14.0, y: image.y.max(cell.y) + 14.0, w: 24.0, h: 24.0 }, idx);
                items.push(Item::Clip(None));
            }
        }
        if self.show_ui {
            self.build_hud(&mut items, vp);
            self.build_status_line(&mut items, vp);
        }

        let animating =
            self.playing || self.badge_flash > 0.0 || !self.started;
        FrameDesc {
            clear: FRAME_BG,
            uploads,
            plate: None,
            items,
            animating,
            redraw_at: if animating {
                None
            } else {
                Some(Instant::now() + std::time::Duration::from_millis(100))
            },
        }
    }

    /// Shared layout for drawing and hit testing; the image transform is always
    /// relative to this canvas, including in mask/crop and side-by-side modes.
    fn workspace(&self) -> Workspace {
        Workspace::new(self.vp, self.show_ui, self.fullscreen)
    }

    fn source_capacity(&self) -> usize {
        (self.workspace().list.h / SOURCE_H).floor().max(1.0) as usize
    }

    fn source_first(&self) -> usize {
        self.source_scroll.min(self.videos.len().saturating_sub(self.source_capacity()))
    }

    fn source_row(&self, index: usize) -> RectPx {
        let l = self.workspace();
        RectPx { x: 10.0, y: l.list.y + (index - self.source_first()) as f32 * SOURCE_H,
            w: l.rail - 20.0, h: SOURCE_H - 6.0 }
    }

    fn controls(&self) -> Vec<Control> {
        let l = self.workspace();
        let mut out = Vec::new();
        let tool = if !self.mask_mode { 0 } else if self.crop.is_none() { 1 } else { 2 };
        let mut x = if self.fullscreen { 16.0 } else { 100.0 };
        for (i, (label, w)) in [("COMPARE", 76.0), ("MASK", 56.0), ("CROP", 56.0)].into_iter().enumerate() {
            out.push(Control::new(RectPx { x, y: 6.0, w, h: 26.0 }, label, tool == i, Action::Tool(i)));
            x += w + 4.0;
        }
        x = l.rail + 12.0;
        let y = HEADER_H + 6.0;
        if !self.mask_mode {
            for (mode, label, w) in [
                (Mode::Overlay, "Single", 62.0), (Mode::SideBySide, "Side by side", 102.0),
                (Mode::Delta, "Difference", 96.0), (Mode::Split, "Wipe", 58.0),
                (Mode::Checker, "Checker", 74.0), (Mode::Blend, "Blend", 64.0),
            ] {
                out.push(Control::new(RectPx { x, y, w, h: 27.0 }, label, self.mode == mode, Action::View(mode)));
                x += w + 4.0;
            }
            if matches!(self.mode, Mode::Delta | Mode::Checker | Mode::Blend) && x + 144.0 < self.vp.0 {
                for (label, up) in [("−", false), ("+", true)] {
                    out.push(Control::new(RectPx { x, y, w: 28.0, h: 27.0 }, label, false, Action::Param(up)));
                    x += 32.0;
                }
            }
        } else if self.crop.is_some() {
            x += 142.0;
            for (label, w, action) in [("Save still + mask", 138.0, Action::Save), ("Export ProRes", 122.0, Action::Export)] {
                out.push(Control::new(RectPx { x, y, w, h: 27.0 }, label, false, action));
                x += w + 8.0;
            }
        } else {
            x += 118.0;
            for (label, w, action) in [("−", 28.0, Action::Brush(false)), ("+", 28.0, Action::Brush(true)), ("Save mask", 100.0, Action::Save)] {
                out.push(Control::new(RectPx { x, y, w, h: 27.0 }, label, false, action));
                x += w + 8.0;
            }
        }
        out
    }

    fn activate(&mut self, action: Action) {
        self.mouse_up();
        match action {
            Action::Tool(0) => {
                if self.mask_mode { self.mask_mode = false; self.leave_mask_mode(); }
            }
            Action::Tool(tool) => {
                if !self.mask_mode { self.key(Key::Char('m')); }
                if (tool == 2) != self.crop.is_some() { self.toggle_crop(); }
            }
            Action::View(mode) => self.mode = mode,
            Action::Brush(up) => self.resize_brush(if up { 1.25 } else { 0.8 }),
            Action::Save => self.save_mask(),
            Action::Export => self.export_crop(),
            Action::Param(up) => self.adjust_param(up),
        }
    }

    /// Persistent transport; these rectangles also own mouse hit testing.
    fn btn_prev(&self) -> RectPx {
        let r = self.workspace().transport;
        RectPx { x: r.x + 18.0, y: r.y + 14.0, w: 26.0, h: 32.0 }
    }
    fn btn_play(&self) -> RectPx {
        let r = self.btn_prev();
        RectPx { x: r.x + 34.0, w: 32.0, ..r }
    }
    fn btn_next(&self) -> RectPx {
        let r = self.btn_play();
        RectPx { x: r.x + 40.0, w: 26.0, ..r }
    }
    fn seek_rect(&self, _vp: (f32, f32)) -> RectPx {
        let r = self.workspace().transport;
        let x = self.btn_next().x + 40.0;
        let reserve = if r.w >= 720.0 { 300.0 } else { 186.0 };
        RectPx { x, y: r.y + 28.0, w: (r.w - (x - r.x) - reserve).max(32.0), h: 4.0 }
    }

    fn build_hud(&self, items: &mut Vec<Item>, vp: (f32, f32)) {
        let l = self.workspace();
        let panel = WORKSPACE_PANEL;
        // Opaque shell surfaces keep zoomed footage out of the interface.
        for r in [RectPx { x: 0.0, y: 0.0, w: vp.0, h: HEADER_H },
            RectPx { x: 0.0, y: HEADER_H, w: l.rail, h: vp.1 - HEADER_H - STATUS_H },
            RectPx { x: l.rail, y: HEADER_H, w: vp.0 - l.rail, h: CONTEXT_H }, l.transport] {
            items.push(Item::Rect(RectItem::new(r, panel)));
        }
        for r in [RectPx { x: 0.0, y: HEADER_H - 1.0, w: vp.0, h: 1.0 },
            RectPx { x: l.rail - 1.0, y: HEADER_H, w: 1.0, h: vp.1 - HEADER_H - STATUS_H },
            RectPx { x: l.rail, y: l.canvas.y - 1.0, w: l.canvas.w, h: 1.0 },
            RectPx { x: l.rail, y: l.transport.y, w: l.canvas.w, h: 1.0 }] {
            items.push(Item::Rect(RectItem::new(r, WORKSPACE_RULE)));
        }
        for control in self.controls() {
            let hovered = contains(control.r, self.cursor.0, self.cursor.1) && self.cursor_inside;
            let tool = matches!(control.action, Action::Tool(_));
            let disabled = matches!(control.action, Action::Save) && self.mask_save.is_some()
                || matches!(control.action, Action::Export) && self.crop_export.is_some();
            if control.selected || hovered {
                items.push(Item::Rect(RectItem { radius: if control.selected { 7.0 } else { 6.0 },
                    ..RectItem::new(control.r, if tool && control.selected { TOOL_BG } else { CONTROL_BG }) }));
            }
            let color = if disabled { WORKSPACE_MUTED } else if tool && control.selected { ACCENT }
                else if control.selected || hovered { WORKSPACE_TEXT } else { WORKSPACE_DIM };
            ui_label(items, control.r.x + control.r.w / 2.0, control.r.y + control.r.h / 2.0,
                11.0, color, &control.label, Align::Center, control.r.w - 8.0);
        }
        if self.mask_mode {
            let label = if let Some(c) = self.crop {
                let m = self.masks[self.active].as_ref().unwrap();
                let (_, _, w, h) = c.pixels(m.width, m.height);
                format!("Crop {w} × {h}")
            } else { format!("Brush {:.0} px", self.brush_diameter) };
            ui_label(items, l.rail + 20.0, HEADER_H + CONTEXT_H / 2.0, 11.0, WORKSPACE_TEXT, label, Align::Left, 126.0);
        } else if vp.0 - l.rail > 760.0 {
            let parameter = match self.mode {
                Mode::Delta => format!("gain {:.1}×", self.gain),
                Mode::Blend => format!("{:.0}%", self.blend * 100.0),
                Mode::Checker => format!("{:.0} px", self.checker_px),
                _ => String::new(),
            };
            ui_label(items, vp.0 - 16.0, HEADER_H + CONTEXT_H / 2.0, 11.0, WORKSPACE_DIM, parameter, Align::Right, 90.0);
        }

        // A scrollable numbered list; filenames are measured/ellipsized by the font renderer.
        items.push(Item::Clip(Some(l.list)));
        let first = self.source_first();
        for i in first..(first + self.source_capacity()).min(self.videos.len()) {
            let v = &self.videos[i];
            let r = self.source_row(i);
            let color = clip_color(i);
            if i == self.active || contains(r, self.cursor.0, self.cursor.1) && self.cursor_inside {
                items.push(Item::Rect(RectItem { radius: 7.0, ..RectItem::new(r, SOURCE_BG) }));
            }
            if i == self.active {
                items.push(Item::Rect(RectItem::new(RectPx { w: 2.0, ..r }, color)));
            }
            number_badge(items, RectPx { x: r.x + 10.0, y: r.y + 12.0, w: 22.0, h: 24.0 }, i);
            let x = r.x + 42.0;
            let width = r.w - 50.0;
            ui_label(items, x, r.y + 18.0, 11.0, WORKSPACE_TEXT, name_of(&v.info.path), Align::Left, width);
            ui_label(items, x, r.y + 39.0, 10.0, WORKSPACE_DIM,
                format!("{}×{}  {:.3} fps", v.info.width, v.info.height, v.info.fps), Align::Left, width);
            ui_label(items, x, r.y + 56.0, 10.0, if v.player.failed() { ERR } else { WORKSPACE_MUTED },
                if v.player.failed() { "DECODE FAILED".into() } else {
                    format!("{}  {}", v.info.codec.to_uppercase(), fmt_time(v.info.duration))
                }, Align::Left, width);
        }
        items.push(Item::Clip(None));
        if self.videos.len() > self.source_capacity() {
            ui_label(items, l.rail - 14.0, l.list.y + l.list.h - 6.0, 9.0, WORKSPACE_MUTED,
                format!("{}–{} / {} · scroll", first + 1, (first + self.source_capacity()).min(self.videos.len()), self.videos.len()), Align::Right, l.rail - 28.0);
        }
        if l.inspector.h > 0.0 {
            let v = &self.videos[self.active];
            items.push(Item::Rect(RectItem::new(RectPx { h: 1.0, ..l.inspector }, WORKSPACE_RULE)));
            ui_label(items, 16.0, l.inspector.y + 24.0, 10.0, WORKSPACE_MUTED,
                format!("INSPECTOR: {}", self.active + 1), Align::Left, l.rail - 32.0);
            for (i, (key, value)) in [
                ("Format", format!("{} {}", v.info.codec, v.info.pix_fmt)),
                ("Resolution", format!("{}×{}", v.info.width, v.info.height)),
                ("Framerate", format!("{:.3} fps", v.info.fps)),
                ("Bitrate", v.info.bit_rate.map(|b| format!("{:.2} Mb/s", b as f64 / 1e6)).unwrap_or_else(|| "—".into())),
                ("Size", fmt_size(v.info.file_size)),
            ].into_iter().enumerate() {
                let y = l.inspector.y + 52.0 + i as f32 * 23.0;
                ui_label(items, 16.0, y, 11.0, WORKSPACE_DIM, key, Align::Left, 78.0);
                ui_label(items, l.rail - 16.0, y, 11.0, WORKSPACE_TEXT, value, Align::Right, l.rail - 110.0);
            }
            let dir = v.info.path.parent().unwrap_or_else(|| std::path::Path::new(""));
            ui_label(items, 16.0, l.inspector.y + 190.0, 10.0, WORKSPACE_MUTED, dir.to_string_lossy(), Align::Left, l.rail - 32.0);
        }
        self.build_transport(items);
    }

    fn build_transport(&self, items: &mut Vec<Item>) {
        let l = self.workspace();
        let prev = self.btn_prev();
        let play = self.btn_play();
        let next = self.btn_next();
        for (r, left) in [(prev, true), (next, false)] {
            let cx = r.x + r.w / 2.0;
            items.push(Item::Triangle { r: RectPx { x: cx - 4.0, y: r.y + 11.0, w: 8.0, h: 10.0 }, color: WORKSPACE_DIM, left, radius: 0.5 });
            items.push(Item::Rect(RectItem::new(RectPx { x: cx + if left { -7.0 } else { 6.0 }, y: r.y + 11.0, w: 1.0, h: 10.0 }, WORKSPACE_DIM)));
        }
        items.push(Item::Rect(RectItem { radius: 16.0, ..RectItem::new(play, ACCENT) }));
        if self.playing {
            for x in [play.x + 11.0, play.x + 18.0] {
                items.push(Item::Rect(RectItem::new(RectPx { x, y: play.y + 10.0, w: 3.0, h: 12.0 }, FRAME_INK)));
            }
        } else {
            items.push(Item::Triangle { r: RectPx { x: play.x + 12.0, y: play.y + 10.0, w: 10.0, h: 12.0 }, color: FRAME_INK, left: false, radius: 1.0 });
        }
        let seek = self.seek_rect(self.vp);
        items.push(Item::Rect(RectItem { radius: 2.0, ..RectItem::new(seek, TRACK) }));
        let fraction = if self.wrap.is_finite() && self.wrap > 0.0 { (self.t / self.wrap).clamp(0.0, 1.0) as f32 } else { 0.0 };
        items.push(Item::Rect(RectItem { radius: 2.0, ..RectItem::new(RectPx { w: seek.w * fraction, ..seek }, ACCENT) }));
        items.push(Item::Rect(RectItem { radius: 6.0, ..RectItem::new(RectPx { x: seek.x + seek.w * fraction - 6.0, y: seek.y - 4.0, w: 12.0, h: 12.0 }, ACCENT) }));
        let cy = l.transport.y + 30.0;
        ui_label(items, seek.x + seek.w + 18.0, cy, 11.0, WORKSPACE_TEXT,
            format!("{} / {}", fmt_time(self.t), if self.wrap.is_finite() { fmt_time(self.wrap) } else { "?".into() }), Align::Left, 166.0);
        if l.transport.w >= 720.0 {
            ui_label(items, self.vp.0 - 18.0, cy, 10.0, WORKSPACE_DIM,
                format!("frame {} · {:.2}×", (self.t * self.fps).round() as i64, self.speed), Align::Right, 120.0);
        }
    }

    fn build_status_line(&self, items: &mut Vec<Item>, vp: (f32, f32)) {
        let y = vp.1 - STATUS_H;
        items.push(Item::Rect(RectItem::new(RectPx { x: 0.0, y, w: vp.0, h: STATUS_H }, WORKSPACE_PANEL)));
        items.push(Item::Rect(RectItem::new(RectPx { x: 0.0, y, w: vp.0, h: 1.0 }, WORKSPACE_RULE)));
        let mode = if !self.mask_mode { "COMPARE" } else if self.crop.is_some() { "CROP" } else { "MASK" };
        ui_label(items, 16.0, y + STATUS_H / 2.0, 10.0, if self.mask_mode { MASK_RED } else { ACCENT }, mode, Align::Left, 70.0);
        let status = if !self.mask_status.is_empty() && self.mask_mode { self.mask_status.clone() }
            else { format!("Clip {} · {}", self.active + 1, if self.zoom <= 1.001 { "fit".into() } else { format!("{:.1}×", self.zoom) }) };
        let hint = if self.mask_mode { if self.crop.is_some() {
                format!("a {}   drag move / resize   s save   e export", ASPECTS[self.aspect].0)
            } else { "drag paint   [ ] brush   s save".into() } }
            else { "1–9 clip   space play   < > step   [ ] speed".into() };
        let hint_width = if vp.0 >= 1000.0 { 440.0 } else { 0.0 };
        ui_label(items, 98.0, y + STATUS_H / 2.0, 10.0, WORKSPACE_DIM, status, Align::Left, vp.0 - 118.0 - hint_width);
        if hint_width > 0.0 {
            ui_label(items, vp.0 - 16.0, y + STATUS_H / 2.0, 10.0, WORKSPACE_MUTED, hint, Align::Right, hint_width);
        }
    }

    /// The launch window: the brand's grid-floor plate, the mark
    /// standing on its horizon with its reflection running out across
    /// the floor, and one line asking for a clip. This is the design
    /// system's `Splash` surface (Abner > Components > Splash); the
    /// geometry notes there and the code here are the same numbers.
    /// (2b's A/B drop targets, terminal hint and keycap legend are gone
    /// for now — one clip already plays, so there is no half-filled pair
    /// to explain.) No video items, so it renders with zero streams
    /// loaded.
    fn launch_frame(&self, vp: (f32, f32)) -> FrameDesc {
        let (w, h) = vp;
        let mut items: Vec<Item> = Vec::new();

        // Below this the plate's vanishing point falls outside the frame
        // and the horizon stops reading, so the window drops back to the
        // bare mark on the flat ground — the Splash card's own rule.
        let splash = w >= SPLASH_MIN_W && h >= SPLASH_MIN_H;

        // The logo image, not type: it already carries the "VIDEO QUALITY
        // TESTING TOOLKIT" line. The renderer owns the aspect, the way it
        // owns glyph metrics; the width is capped against the window so a
        // narrow one doesn't run it edge to edge.
        let lw = if splash {
            (w * LOCKUP_W).clamp(320.0, 980.0).min(w - LOCKUP_CLEAR * 2.0)
        } else {
            (w * 0.34).clamp(240.0, 460.0).min(w - 96.0)
        };
        let lh = lw / self.logo_aspect;
        // The card is drawn on a 640px frame with a 391px lockup; its
        // shadow offsets are in that lockup's px and scale with the mark.
        let card = lw / 391.0;
        let gap = 36.0;

        // The plate is cover-fitted, then slid until its lit line sits at
        // HORIZON_Y — clamped so the fit can never open a gap, and the
        // LAYOUT follows wherever that leaves the line rather than
        // assuming it landed where it was asked to.
        let mut horizon = h * HORIZON_Y;
        let mut floor_h = 0.0;
        if splash {
            let (pw, ph) = self.plate_size;
            let k = (w / pw).max(h / ph);
            let (dw, dh) = (pw * k, ph * k);
            let y = (h * HORIZON_Y - dh * self.plate_horizon).clamp(h - dh, 0.0);
            horizon = y + dh * self.plate_horizon;
            items.push(Item::Plate {
                r: RectPx { x: (w - dw) / 2.0, y, w: dw, h: dh },
                alpha: LAUNCH_BG[3],
            });
            // The reflection lies flat on the floor BEYOND the horizon,
            // hinged where the mark meets it. Its quad is the projected
            // footprint: the plane widens by `k` at the far end, which is
            // also how the shader recovers the mark's own width.
            let persp = lw * FLOOR_PERSP;
            let spread = persp / (persp - lh * FLOOR_TILT.sin());
            let (fw, fh) = (lw * spread, lh * FLOOR_TILT.cos() * spread);
            floor_h = fh;
            items.push(Item::LogoFloor {
                r: RectPx { x: (w - fw) / 2.0, y: horizon + lh * FLOOR_DROP, w: fw, h: fh },
                alpha: FLOOR_ALPHA,
                persp,
                tilt: FLOOR_TILT,
                src_h: lh,
            });
            // Pulls the haze in from the corners, so the colour stays
            // round the mark instead of running to the window's edge.
            items.push(Item::Vignette { r: RectPx { x: 0.0, y: 0.0, w, h }, color: VOID });
            // Two grounds: the traffic lights float over the haze at the
            // top, the message sits on the grid at the bottom, and type
            // over either drops below 4.5:1 without them.
            items.push(Item::Rect(RectItem {
                fade_down: true,
                ..RectItem::new(RectPx { x: 0.0, y: 0.0, w, h: h * 0.255 }, SPLASH_SCRIM)
            }));
            items.push(Item::Rect(RectItem {
                fade_up: true,
                ..RectItem::new(RectPx { x: 0.0, y: h * 0.72, w, h: h * 0.28 }, SPLASH_SCRIM)
            }));

        }

        // Standing on the line, not floating over it.
        let top = if splash {
            horizon - lh * LOCKUP_LIFT - lh
        } else {
            (h - (lh + gap + FOOT_H)) / 2.0
        };
        let mark = RectPx { x: (w - lw) / 2.0, y: top, w: lw, h: lh };
        if splash {
            for (dy, blur, alpha) in SHADOW_PASSES {
                items.push(Item::LogoShadow { mark, dy: dy * card, blur: blur * card, alpha });
            }
        }
        items.push(Item::Logo { r: mark, alpha: 1.0 });
        if splash {
            // Over the art, under the chrome and the type.
            items.push(Item::Scanlines { r: RectPx { x: 0.0, y: 0.0, w, h }, color: SCANLINE });
        }

        // The lamp and word at the top right, level with the traffic
        // lights.
        let sy = if self.fullscreen { 14.0 } else { TITLEBAR_H / 2.0 };
        let label = TextItem {
            align: Align::Right,
            valign: VAlign::Middle,
            tracking: 10.0 * 0.09,
            ..TextItem::new(w - 16.0, sy, 10.0, INK_MUTED, "READY")
        };
        let lamp_x = w - 16.0 - MONO_ADV * 10.0 * 1.09 * 5.0 - 8.0 - 7.0;
        items.push(Item::Rect(RectItem {
            radius: 6.5,
            ..RectItem::new(RectPx { x: lamp_x - 3.0, y: sy - 6.5, w: 13.0, h: 13.0 }, LAMP_GLOW)
        }));
        items.push(Item::Rect(RectItem {
            radius: 3.5,
            ..RectItem::new(RectPx { x: lamp_x, y: sy - 3.5, w: 7.0, h: 7.0 }, LAMP)
        }));
        items.push(Item::Text(label));

        // The foot: a hairline, the one instruction, and the accepted
        // formats between a blue and a red tick. A drag over the window
        // lights the instruction — the whole window is the target (winit
        // gives no drop position anyway).
        let (msg, col) = if self.drag_hover {
            ("release to open", ACCENT)
        } else {
            ("drop a video file to begin", INSTRUCTION)
        };
        let (rule_h, fmt_px, msg_px, step) = (1.0, 11.0, 13.0, 13.0);
        // On the plate the foot sits on the near floor, past the
        // reflection, where the ground is closest to black.
        let foot = if splash {
            let bottom = h - h * FOOT_BOTTOM;
            (bottom - (rule_h + msg_px + fmt_px + step * 2.0)).max(horizon + lh * FLOOR_DROP + floor_h * 0.5)
        } else {
            top + lh + gap
        };
        items.push(Item::Rect(RectItem {
            fade_x: true,
            ..RectItem::new(RectPx { x: w / 2.0 - 150.0, y: foot, w: 300.0, h: rule_h }, RULE)
        }));
        let msg_y = foot + rule_h + step;
        items.push(Item::Text(TextItem {
            align: Align::Center,
            tracking: msg_px * 0.16,
            ..TextItem::new(w / 2.0, msg_y, msg_px, col, msg)
        }));
        let fmt_y = msg_y + msg_px + step + fmt_px / 2.0;
        let fmt_track = fmt_px * 0.09;
        let fmt_w = FORMATS.chars().count() as f32 * (MONO_ADV * fmt_px + fmt_track);
        items.push(Item::Text(TextItem {
            align: Align::Center,
            valign: VAlign::Middle,
            tracking: fmt_track,
            ..TextItem::new(w / 2.0, fmt_y, fmt_px, INK_MUTED, FORMATS)
        }));
        for (x, c) in [(w / 2.0 - fmt_w / 2.0 - 10.0 - 26.0, BRAND_BLUE), (w / 2.0 + fmt_w / 2.0 + 10.0, BRAND_RED)] {
            items.push(Item::Rect(RectItem::new(RectPx { x, y: fmt_y - 1.0, w: 26.0, h: 2.0 }, c)));
        }

        FrameDesc {
            clear: LAUNCH_BG,
            uploads: Vec::new(),
            plate: None,
            items,
            animating: false,
            redraw_at: None,
        }
    }
}

#[derive(Clone, Copy)]
struct Workspace {
    rail: f32,
    canvas: RectPx,
    list: RectPx,
    inspector: RectPx,
    transport: RectPx,
}
impl Workspace {
    fn new(vp: (f32, f32), visible: bool, _fullscreen: bool) -> Self {
        let (w, h) = vp;
        let rail = if !visible { 0.0 } else if w < 900.0 { 210.0_f32.min(w * 0.3) } else { 280.0 };
        let header = if visible { HEADER_H + CONTEXT_H } else { 0.0 };
        let bottom = if visible { TRANSPORT_H + STATUS_H } else { 0.0 };
        let canvas = RectPx { x: rail, y: header, w: (w - rail).max(1.0), h: (h - header - bottom).max(1.0) };
        let inspector_h = if visible && h >= 600.0 { 216.0 } else { 0.0 };
        let inspector = RectPx { x: 0.0, y: h - STATUS_H - inspector_h, w: rail, h: inspector_h };
        Self {
            rail, canvas, inspector,
            list: RectPx { x: 0.0, y: HEADER_H + 12.0, w: rail, h: (inspector.y - HEADER_H - 24.0).max(SOURCE_H) },
            transport: RectPx { x: rail, y: h - STATUS_H - TRANSPORT_H, w: w - rail, h: TRANSPORT_H },
        }
    }
}

#[derive(Clone, Copy)]
enum Action { Tool(usize), View(Mode), Brush(bool), Save, Export, Param(bool) }
struct Control { r: RectPx, label: String, selected: bool, action: Action }
impl Control {
    fn new(r: RectPx, label: &str, selected: bool, action: Action) -> Self {
        Self { r, label: label.into(), selected, action }
    }
}
fn contains(r: RectPx, x: f32, y: f32) -> bool {
    x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h
}
fn inset_rect(r: RectPx, pad: f32) -> RectPx {
    let pad = pad.min((r.w - 1.0).max(0.0) / 2.0).min((r.h - 1.0).max(0.0) / 2.0);
    RectPx { x: r.x + pad, y: r.y + pad, w: r.w - pad * 2.0, h: r.h - pad * 2.0 }
}
fn ui_label(items: &mut Vec<Item>, x: f32, y: f32, px: f32, color: [f32; 4], text: impl Into<String>, align: Align, width: f32) {
    items.push(Item::Text(TextItem { align, valign: VAlign::Middle, max_width: Some(width.max(0.0)),
        ..TextItem::new(x, y, px, color, text) }));
}
fn hex_color(rgb: u32) -> [f32; 4] {
    [((rgb >> 16) & 255) as f32 / 255.0, ((rgb >> 8) & 255) as f32 / 255.0, (rgb & 255) as f32 / 255.0, 1.0]
}
fn clip_color(index: usize) -> [f32; 4] {
    const COLORS: [u32; 9] = [0x61b5ee, 0xe6ac69, 0xb89ae8, 0x6dc7aa, 0xe88891, 0xc9c970, 0x70c8d2, 0xdb9bc6, 0xa4b9d4];
    hex_color(COLORS[index % COLORS.len()])
}
fn number_badge(items: &mut Vec<Item>, r: RectPx, index: usize) {
    let color = clip_color(index);
    let bg = [color[0] * 0.17 + 0.04, color[1] * 0.17 + 0.04, color[2] * 0.17 + 0.04, 1.0];
    items.push(Item::Rect(RectItem { radius: 5.0, border_w: 1.0,
        border_color: [color[0], color[1], color[2], 0.12], ..RectItem::new(r, bg) }));
    ui_label(items, r.x + r.w / 2.0, r.y + r.h / 2.0, 11.0, color, (index + 1).to_string(), Align::Center, r.w - 4.0);
}
const HEADER_H: f32 = 38.0;
const CONTEXT_H: f32 = 40.0;
const SOURCE_H: f32 = 80.0;
const WORKSPACE_PANEL: [f32; 4] = [0.0196, 0.0196, 0.0235, 1.0];
const CANVAS_BG: [f32; 4] = [29.0 / 255.0, 27.0 / 255.0, 27.0 / 255.0, 1.0];
const WORKSPACE_TEXT: [f32; 4] = [209.0 / 255.0, 209.0 / 255.0, 209.0 / 255.0, 1.0];
const WORKSPACE_DIM: [f32; 4] = [0.57, 0.57, 0.57, 1.0];
const WORKSPACE_MUTED: [f32; 4] = [0.43, 0.43, 0.43, 1.0];
const WORKSPACE_RULE: [f32; 4] = [0.09, 0.09, 0.09, 1.0];
const CONTROL_BG: [f32; 4] = [26.0 / 255.0, 26.0 / 255.0, 26.0 / 255.0, 1.0];
const SOURCE_BG: [f32; 4] = [0.082, 0.082, 0.082, 1.0];
const TOOL_BG: [f32; 4] = [0.043, 0.098, 0.149, 1.0];

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::{Duration, Instant};

    fn pointer_down(app: &mut App, x: f32, y: f32) {
        let r = app.content_rect(app.active);
        let v = &app.videos[app.active];
        app.mouse_down(r.x + x * r.w / v.info.width as f32, r.y + y * r.h / v.info.height as f32);
    }
    fn pointer_move(app: &mut App, x: f32, y: f32) {
        let r = app.content_rect(app.active);
        let v = &app.videos[app.active];
        app.cursor_moved(r.x + x * r.w / v.info.width as f32, r.y + y * r.h / v.info.height as f32);
    }

    #[test]
    fn workspace_controls_select_tools_and_never_paint_the_shell() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 3);
        let click = |app: &mut App, action: fn(Action) -> bool| {
            let r = app.controls().into_iter().find(|c| action(c.action)).unwrap().r;
            app.mouse_down(r.x + r.w / 2.0, r.y + r.h / 2.0);
            app.mouse_up();
        };
        click(&mut app, |a| matches!(a, Action::View(Mode::Blend)));
        assert_eq!(app.mode, Mode::Blend);
        click(&mut app, |a| matches!(a, Action::Tool(2)));
        assert!(app.mask_mode && app.crop.is_some() && !app.playing);
        let crop = app.crop;
        // A zoomed crop can extend underneath the shell, but the shell owns input.
        app.zoom = 4.0;
        app.mouse_down(120.0, app.workspace().inspector.y + 30.0);
        app.cursor_moved(130.0, app.workspace().inspector.y + 40.0);
        app.mouse_up();
        assert_eq!(app.crop, crop);
        click(&mut app, |a| matches!(a, Action::Tool(1)));
        assert!(app.mask_mode && app.crop.is_none());
        let r = app.source_row(1);
        app.mouse_down(r.x + 60.0, r.y + 25.0);
        app.mouse_up();
        assert_eq!(app.active, 1);
        assert_eq!(app.masks[1].as_ref().unwrap().revision, 0);
        app.mouse_down(300.0, HEADER_H + 20.0);
        app.cursor_moved(350.0, HEADER_H + 20.0);
        app.mouse_up();
        assert_eq!(app.masks[1].as_ref().unwrap().revision, 0);
        let brush = app.brush_diameter;
        click(&mut app, |a| matches!(a, Action::Brush(true)));
        assert!(app.brush_diameter > brush);
        click(&mut app, |a| matches!(a, Action::Tool(0)));
        assert!(!app.mask_mode);
        assert!(app.videos.iter().all(|v| v.last_frame.is_none()));
        assert_eq!(app.mode, Mode::Blend);
    }

    #[test]
    fn workspace_geometry_and_scroll_keep_every_clip_reachable() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 9);
        for vp in [(720.0, 480.0), (900.0, 520.0), (1280.0, 800.0)] {
            app.vp = vp;
            let l = app.workspace();
            assert_eq!(l.canvas.x, l.rail);
            assert_eq!(l.canvas.y + l.canvas.h, l.transport.y);
            for c in app.controls() {
                assert!(c.r.x + c.r.w <= vp.0, "control overflow at {vp:?}: {}", c.label);
            }
            app.key(Key::Char('9'));
            assert_eq!(app.active, 8);
            assert!(app.source_first() <= 8 && app.source_first() + app.source_capacity() > 8);
            let r = app.source_row(8);
            assert!(r.y + r.h <= l.list.y + l.list.h);
            app.cursor_moved(20.0, l.list.y + 12.0);
            app.scroll(0.0, 40.0);
            assert!(app.source_first() < 8);
        }
        app.key(Key::Tab);
        let c = app.workspace().canvas;
        assert_eq!((c.x, c.y, c.w, c.h), (0.0, 0.0, 1280.0, 800.0));
    }

    #[test]
    fn side_by_side_zoom_anchors_inside_the_correct_workspace_cell() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 3);
        app.mode = Mode::SideBySide;
        let r = app.content_rect(1);
        let pointer = (r.x + r.w * 0.6, r.y + r.h * 0.4);
        app.cursor_moved(pointer.0, pointer.1);
        app.pinch(0.5);
        let zoomed = app.content_rect(1);
        assert!(((pointer.0 - zoomed.x) / zoomed.w - 0.6).abs() < 0.001);
        assert!(((pointer.1 - zoomed.y) / zoomed.h - 0.4).abs() < 0.001);
        let old = app.zoom;
        app.cursor_moved(80.0, HEADER_H + 80.0);
        app.pinch(0.5);
        assert_eq!(app.zoom, old, "pinching the source list must not zoom the image");
    }

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
        // Map image pixels through the workspace canvas.
        pointer_down(&mut app, 160.0, 90.0);
        pointer_move(&mut app, 200.0, 90.0);
        app.mouse_up();
        let mask = app.masks[0].as_ref().unwrap();
        assert_eq!(mask.pixels[90 * 320 + 160], 255);
        assert_eq!(mask.pixels[90 * 320 + 180], 255);
        assert_eq!(mask.pixels[90 * 320 + 200], 255);
        assert_eq!(mask.pixels[70 * 320 + 180], 0);
        // Pinch in mask mode must use the full-window fit, not the SBS cell.
        pointer_move(&mut app, 160.0, 90.0);
        app.pinch(1.0);
        assert_eq!(app.center, (0.5, 0.5));
        pointer_down(&mut app, 160.0, 110.0);
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
        pointer_down(&mut app, 160.0, 90.0);
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

    /// The crop marquee: it starts on the whole frame, the corners resize
    /// and the body moves (both staying inside the image), it takes the
    /// pointer away from the brush while it is up, and S writes the mask
    /// and the video pixels cut to the SAME rectangle — that pairing is
    /// the whole point of the feature.
    #[test]
    fn crop_marquee_drags_and_exports_mask_and_video_at_one_size() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 2);
        app.key(Key::Char('m'));
        // The export needs real pixels, so wait for a decoded frame.
        assert!(
            tick_until(&mut app, Duration::from_secs(10), |a| a.videos[0].last_frame.is_some()),
            "mask mode never captured a frame to crop"
        );
        // The crop and video share the workspace image transform.
        app.key(Key::Char('c'));
        assert_eq!(app.crop, Some(Crop { x: 0.0, y: 0.0, w: 320.0, h: 180.0 }));
        assert!(!app.brush_cursor_visible(), "the marquee owns the pointer");

        // Drag the SE handle in to a quarter frame.
        pointer_down(&mut app, 320.0, 180.0);
        pointer_move(&mut app, 160.0, 90.0);
        app.mouse_up();
        assert_eq!(app.crop, Some(Crop { x: 0.0, y: 0.0, w: 160.0, h: 90.0 }));

        // Drag the body far past the corner: it keeps its size and stops
        // at the edge rather than leaving the image.
        pointer_down(&mut app, 80.0, 45.0);
        app.cursor_moved(4000.0, 4000.0);
        app.mouse_up();
        assert_eq!(app.crop, Some(Crop { x: 160.0, y: 90.0, w: 160.0, h: 90.0 }));

        // A press inside paints nothing while the marquee is up.
        pointer_down(&mut app, 250.0, 140.0);
        pointer_move(&mut app, 275.0, 145.0);
        app.mouse_up();
        assert_eq!(app.masks[0].as_ref().unwrap().revision, 0);

        let video = std::env::temp_dir().join(format!("abner-crop-{}.mp4", std::process::id()));
        app.videos[0].info.path = video.clone();
        app.key(Key::Char('s'));
        assert!(tick_until(&mut app, Duration::from_secs(5), |a| a.mask_save.is_none()));
        assert!(app.mask_status.starts_with("Saved"), "{}", app.mask_status);
        let (mask_png, crop_png) = (mask::output_path(&video), mask::crop_output_path(&video));
        let read = |path: &PathBuf| {
            let mut reader = png::Decoder::new(std::io::BufReader::new(
                std::fs::File::open(path).unwrap(),
            ))
            .read_info()
            .unwrap();
            let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
            let info = reader.next_frame(&mut pixels).unwrap();
            (info.width, info.height, info.color_type)
        };
        let cut = read(&mask_png);
        assert_eq!(cut, (160, 90, png::ColorType::Grayscale));
        assert_eq!(read(&crop_png), (160, 90, png::ColorType::Rgba));
        std::fs::remove_file(mask_png).unwrap();
        std::fs::remove_file(crop_png).unwrap();

        // Hiding the marquee gives the brush back — and the mask its
        // full size, since the crop is gone with it.
        app.key(Key::Char('c'));
        assert_eq!(app.crop, None);
        pointer_down(&mut app, 160.0, 90.0);
        app.mouse_up();
        assert!(app.masks[0].as_ref().unwrap().revision > 0);
        // [ and ] size the brush.
        let brush = app.brush_diameter;
        app.key(Key::Char(']'));
        assert!(app.brush_diameter > brush);
        app.key(Key::Char('['));
        assert!((app.brush_diameter - brush).abs() < 0.01);
        // Leaving mask mode lets the retained frames go.
        app.key(Key::Char('m'));
        assert!(app.videos.iter().all(|v| v.last_frame.is_none()));
    }

    /// `A` steps the ratio presets: the marquee reshapes about its centre,
    /// a corner drag keeps the ratio, and Shift-A walks back to free.
    #[test]
    fn crop_aspect_presets_reshape_and_lock_the_marquee() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 1);
        app.start_crop(None);
        // 320x180 → 1:1 lands on the largest centred square.
        for _ in 0..4 { app.key(Key::Char('a')); }
        assert_eq!(ASPECTS[app.aspect].0, "ratio 1:1");
        assert_eq!(app.crop, Some(Crop { x: 70.0, y: 0.0, w: 180.0, h: 180.0 }));
        // Drag the SE handle (screen 4·250, 40 + 4·180) in: stays square.
        pointer_down(&mut app, 250.0, 180.0);
        pointer_move(&mut app, 175.0, 115.0);
        app.mouse_up();
        let c = app.crop.unwrap();
        assert!((c.w - c.h).abs() < 1e-3 && c.w < 180.0, "{c:?}");
        // Every preset snaps to its largest fit — no ratchet from the drag.
        app.key(Key::Char('a'));
        assert_eq!(ASPECTS[app.aspect].0, "ratio 2.39:1");
        let wide = app.crop.unwrap();
        assert!((wide.w - 320.0).abs() < 1e-3 && (wide.w / wide.h - 2.39).abs() < 1e-3, "{wide:?}");
        // On to free: the rect stays, the lock goes; Shift-A walks back.
        app.key(Key::Char('a'));
        assert_eq!(app.aspect, 0);
        assert_eq!(app.crop, Some(wide));
        app.key(Key::Char('A'));
        assert_eq!(ASPECTS[app.aspect].0, "ratio 2.39:1");
    }

    /// `E` with the marquee up re-encodes the whole clip, cut to it, as
    /// ProRes 422 Proxy beside the source — odd sides floored to even.
    #[test]
    fn crop_export_writes_a_prores_proxy_at_the_marquee_size() {
        let Some(clip) = test_clip() else { return };
        let dir = std::env::temp_dir().join(format!("abner-export-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let source = dir.join("clip.mp4");
        std::fs::copy(&clip, &source).unwrap();
        let mut app = mk_app(&source, 1);
        app.start_crop(Some([10.0, 20.0, 101.0, 61.0]));
        assert!(app.crop.is_some());
        app.key(Key::Char('e'));
        assert!(app.crop_export.is_some());
        assert!(tick_until(&mut app, Duration::from_secs(30), |a| a.crop_export.is_none()));
        assert!(app.mask_status.starts_with("Exported"), "{}", app.mask_status);
        let out = mask::crop_video_path(&source);
        assert_eq!(out, dir.join("clip.crop.mov"));
        let probe = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries",
                   "stream=codec_name,profile,width,height", "-of", "csv=p=0"])
            .arg(&out)
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&probe.stdout).trim(), "prores,Proxy,100,60");
        std::fs::remove_dir_all(dir).unwrap();
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
        Video { info, player, shown_pts: 0.0, delivered: false, pending: false, last_frame: None }
    }

    fn mk_app(clip: &PathBuf, n: usize) -> App {
        App::new((0..n).map(|_| mk_video(clip)).collect(), &crate::config::Config::default())
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

    /// ⌘W closes the focused clip: survivors keep playing in sync from
    /// the same moment, and closing the last returns to the launch window.
    #[test]
    fn cmd_w_closes_the_focused_clip() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 3);
        assert!(tick_until(&mut app, Duration::from_secs(10), |a| a.started && a.t > 0.5));
        app.key(Key::Char('3'));
        app.key(Key::Close);
        assert_eq!(app.videos.len(), 2);
        assert_eq!(app.active, 1, "focus clamps to the new last slot");
        assert!(app.take_cmds().iter().any(|c| matches!(c, Cmd::VideosChanged)));
        let t0 = app.t;
        assert!(t0 > 0.4, "closing a clip must not rewind the survivors");
        assert!(tick_until(&mut app, Duration::from_secs(10), |a| a.started && a.t > t0 + 0.3));
        let (p0, p1) = (app.videos[0].shown_pts, app.videos[1].shown_pts);
        assert!((p0 - p1).abs() < 1.5 / app.fps, "survivors drifted: {p0} vs {p1}");

        app.key(Key::Close);
        app.key(Key::Close);
        assert!(!app.ready(), "closing the last clip is the launch window");
        app.take_cmds();
        app.key(Key::Close);
        assert!(app.take_cmds().iter().any(|c| matches!(c, Cmd::Quit)), "⌘W on empty quits");
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
/// The crop marquee's ratio presets, `A` to step through: (the keycap's
/// label, w/h). Image pixels are taken as square, like everywhere else.
const ASPECTS: [(&str, Option<f32>); 6] = [
    ("ratio free", None),
    ("ratio 16:9", Some(16.0 / 9.0)),
    ("ratio 9:16", Some(9.0 / 16.0)),
    ("ratio 4:3", Some(4.0 / 3.0)),
    ("ratio 1:1", Some(1.0)),
    ("ratio 2.39:1", Some(2.39)),
];

const ACCENT: [f32; 4] = [0.082, 0.502, 0.871, 1.0];
/// Frame background / ink on the accent (#050506). Opaque: the window
/// no longer lets the desktop through.
const FRAME_BG: [f32; 4] = [0.0196, 0.0196, 0.0235, 1.0];
const FRAME_INK: [f32; 4] = [0.0196, 0.0196, 0.0235, 1.0];
const ERR: [f32; 4] = [1.0, 0.35, 0.3, 1.0];
const TRACK: [f32; 4] = [1.0, 1.0, 1.0, 0.14];
const MASK_RED: [f32; 4] = [0.906, 0.106, 0.141, 1.0];

// Launch window — the design system's `Splash` surface.
const LAUNCH_BG: [f32; 4] = [0.027, 0.027, 0.035, 1.0];
/// Smallest window that still gets the plate. Under it the vanishing
/// point leaves the frame and the horizon stops reading, so the launch
/// window falls back to the mark centred on the flat ground.
const SPLASH_MIN_W: f32 = 900.0;
const SPLASH_MIN_H: f32 = 520.0;
/// Where the plate's horizon is driven to, down the window.
const HORIZON_Y: f32 = 0.578;
/// Clear space each side of the mark at the plate's size.
const LOCKUP_CLEAR: f32 = 80.0;
/// The lockup's width, as a fraction of the frame (the card's 61%).
const LOCKUP_W: f32 = 0.61;
/// The mark stands ON the line: its baseline sits this fraction of its
/// own height above it — the card's 5px under a 130px lockup.
const LOCKUP_LIFT: f32 = 0.04;
/// Three stacked shadows, (dy, blur, alpha) in the card lockup's px: a
/// tight contact shadow, a wider fall, and an ambient halo that
/// separates the metal from the lit horizon. Tighter than the card's
/// (6/9, 14/20, 0/16): at the card's drops the tagline's shadow parts
/// from the tagline and reads as a second, blurred line of type. Alphas
/// run above the card's for the linear-blending reason the scrims do.
const SHADOW_PASSES: [(f32, f32, f32); 3] = [(2.0, 4.0, 0.80), (5.0, 10.0, 0.55), (0.0, 9.0, 0.65)];
/// The floor projection: pinhole distance as a multiple of the mark's
/// width, the floor's tilt from the screen plane, and how much light
/// the floor gives back.
const FLOOR_PERSP: f32 = 1.36;
const FLOOR_TILT: f32 = 1.0472; // 60° — shallower than the card's 72°, so it runs further toward the viewer
const FLOOR_ALPHA: f32 = 0.15;
/// How far below the horizon the reflection starts, as a fraction of the
/// mark's height — a gap of floor between the tagline and its mirror,
/// so the two never read as one doubled line.
const FLOOR_DROP: f32 = 0.10;
/// The brand's `void` (#06070a): the ground every Splash layer darkens to.
const VOID: [f32; 4] = [0.024, 0.027, 0.039, 1.0];
/// Ground under the traffic lights and under the message.
const SPLASH_SCRIM: [f32; 4] = [0.024, 0.027, 0.039, 0.80];
/// The brand's `scanline` token is white at 0.07, composited in sRGB;
/// blended in linear space that would glow, so it runs far lower here.
const SCANLINE: [f32; 4] = [1.0, 1.0, 1.0, 0.012];
/// How far the foot's last line sits off the bottom edge, as a fraction
/// of the frame (the card's 41px of 400).
const FOOT_BOTTOM: f32 = 0.10;
/// The foot's height (rule, instruction, formats and the two steps
/// between them) — what centres the lockup in the bare fallback.
const FOOT_H: f32 = 51.0;
/// The instruction (#c3ccd6), the design system's `ink-muted` for the
/// labels, and the hairline above them (#616b7b at half).
const INSTRUCTION: [f32; 4] = [0.765, 0.800, 0.839, 1.0];
const INK_MUTED: [f32; 4] = [0.604, 0.651, 0.706, 1.0];
const RULE: [f32; 4] = [0.380, 0.420, 0.482, 0.5];
/// The logo's own blue and red (the brand's `blue` #00adfa and `red`
/// #fc0a1c), for the ticks either side of the formats.
const BRAND_BLUE: [f32; 4] = [0.0, 0.678, 0.980, 1.0];
const BRAND_RED: [f32; 4] = [0.988, 0.039, 0.110, 1.0];
/// The status lamp (`green` #16d97e) and its glow.
const LAMP: [f32; 4] = [0.086, 0.851, 0.494, 1.0];
const LAMP_GLOW: [f32; 4] = [0.086, 0.851, 0.494, 0.07];
/// What the launch window says it opens.
const FORMATS: &str = "MP4 · MOV · MKV · Y4M";

// Crop marquee. The dimmer runs high for the reason the HUD's panels do
// (see shader.wgsl): linear-space blending means a modest alpha barely
// touches bright footage, and this one has to read as "not exported".
const CROP_DIM: [f32; 4] = [0.0, 0.0, 0.0, 0.95];
const CROP_LINE: [f32; 4] = [1.0, 1.0, 1.0, 0.95];
const CROP_INK: [f32; 4] = [0.0, 0.0, 0.0, 0.75];
const CROP_LINE_W: f32 = 1.5;
const CROP_DASH: f32 = 7.0;
const CROP_GAP: f32 = 5.0;
const CROP_HANDLE: f32 = 9.0;
/// Half-size of a corner's grab square — a little wider than it is drawn.
const CROP_GRAB: f32 = 11.0;


/// Transport strip: 13px pad + 32px controls + 11px gap + the status line.
const TRANSPORT_H: f32 = 60.0;
/// Persistent footer for tool, selection, zoom, export results and shortcut hints.
const STATUS_H: f32 = 28.0;
/// Extra grab margin above/below the 5px seek track.
const SEEK_GRAB: f32 = 9.0;
/// Advance width of the monospace UI font, in em — used only to step
/// between right-aligned status segments and keycap chips, never to
/// place a glyph (the renderer measures those exactly).
const MONO_ADV: f32 = 0.60;

/// Logical height of a standard macOS titlebar. The window has no visible
/// bar (`set_titlebar_glass` makes it transparent and runs the content
/// under it), but the traffic-light buttons still float in that strip, so
/// launch content clears them. The loaded workspace uses HEADER_H instead.
const TITLEBAR_H: f32 = 28.0;

fn name_of(path: &std::path::Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
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
