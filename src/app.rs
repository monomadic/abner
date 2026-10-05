//! App state and per-frame logic: the master clock, view modes, input,
//! and the UI overlay.
//!
//! Sync model: one master time `t` advances by wall-clock dt while
//! playing; every player queues `(pts, rgba)` frames and each frame the
//! app pops everything `pts <= t` (newest wins). All streams answer to
//! the same clock, so switching the displayed video (Enter) can never
//! jump in time — the other stream was already decoding the same moment.

use std::time::Instant;

use crate::cut::{Cut, Snap};
use crate::player::Player;
use crate::mask::{self, Corner, Crop, Edge, Mask};
use crate::probe::VideoInfo;
use crate::recent;
use crate::render::{
    Align, FrameDesc, Item, RectItem, RectPx, TextBg, TextItem, ThumbUpload, Upload, VAlign,
    VideoMode,
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
    /// ⌘ plus a digit: on the launch window, open that recent file; with
    /// clips up it is the bare digit (pick that clip), as it always was.
    CmdDigit(char),
    /// ⌘Z: cut mode's undo.
    Undo,
    /// ⇧← / ⇧→: cut mode's previous / next keyframe. Plain arrows elsewhere.
    KeyLeft,
    KeyRight,
}

#[derive(Debug, Clone, Copy)]
pub enum Cmd {
    Quit,
    ToggleFullscreen,
    /// The clip list changed shape outside a drop (⌘W): the runner
    /// re-syncs the GPU's per-slot textures and the window title.
    VideosChanged,
    /// Open the launch window's nth recent tile (a click or ⌘1–4). The
    /// runner loads it through the drop path, so it is recorded again.
    OpenRecent(usize),
}

/// What a press on the crop marquee took hold of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CropGrab {
    /// Anywhere inside: slide the whole rect, size unchanged.
    Move,
    /// One of the white squares: drag it, opposite corner anchored.
    Corner(Corner),
    /// One of the side squares: drag it along its axis, opposite side anchored.
    Edge(Edge),
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum CutDrag {
    /// On the lanes: the playhead follows the pointer.
    Scrub,
    /// On a clip's grip: that boundary (its source time) follows the pointer.
    Edge(f64),
    /// On the zoom slider.
    Zoom,
}

/// The pointer shape the crop marquee wants under the cursor; main.rs maps it
/// to the window's cursor icon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CropCursor {
    /// The dimmed area outside the marquee.
    Crosshair,
    Grab,
    Grabbing,
    /// ↖↘ (top-left / bottom-right corners).
    NwseResize,
    /// ↗↙ (top-right / bottom-left corners).
    NeswResize,
    NsResize,
    EwResize,
    /// Over a recent tile on the launch window: it opens on click.
    Pointer,
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
    /// When and where the last marquee press landed, for double-click.
    last_click: Option<(std::time::Instant, (f32, f32))>,
    /// Index into `ASPECTS` (`A` cycles): the marquee's locked ratio, 0 for
    /// free. A tool setting, so it outlives hiding the marquee.
    aspect: usize,
    /// Cut mode (`T`): the timeline edit of ONE clip. The model outlives
    /// leaving the mode, so a detour through Input doesn't lose the edit;
    /// it is dropped when the clip list changes.
    cut: Option<Cut>,
    cut_mode: bool,
    /// The inspector's tab (0 chapters, 1 streams) and its chapter view.
    cut_tab: usize,
    cut_thumbs: bool,
    /// What a press on the timeline took hold of.
    cut_drag: Option<CutDrag>,
    /// The clock at the last tick, so the view pages after the playhead
    /// only when it moved (and not back from wherever the user scrolled).
    cut_last_t: f64,
    /// `--cut`'s ranges, waiting for the keyframe scan.
    cut_boot: Vec<f64>,
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
    /// False for launches with arguments: the empty window shows only the logo.
    video_splash: bool,
    /// The launch backdrop video (none when the asset is missing or failed
    /// to decode: the still plate stays), and its decoder while
    /// the launch window is up.
    backdrop_src: Option<VideoInfo>,
    backdrop: Option<Backdrop>,
    /// The window is occluded: the backdrop's clock stops, so nothing
    /// drains its queue and backpressure parks the decoder.
    hidden: bool,
    /// The launch window's recent row (`recent.rs`).
    recent: RecentRow,
    cmds: Vec<Cmd>,
}

/// One recent file on its way to (or on) the launch window.
struct RecentTile {
    path: std::path::PathBuf,
    /// Filled in by the thumbnail worker, with the frame.
    info: Option<VideoInfo>,
    rgba: Option<Vec<u8>>,
    /// The worker gave up (moved, deleted, unreadable): the tile drops out.
    failed: bool,
}

/// The recent row: candidates in recency order, the thumbnail workers
/// still out, and what the GPU's atlas cells currently hold. More
/// candidates than tiles are probed, so a file that has gone away is
/// replaced by the next one instead of leaving a hole.
#[derive(Default)]
struct RecentRow {
    /// The list as last read (`set_recent`); `stale` until the launch
    /// window picks it up.
    paths: Vec<std::path::PathBuf>,
    stale: bool,
    tiles: Vec<RecentTile>,
    workers: Vec<std::sync::mpsc::Receiver<recent::ThumbResult>>,
    /// Which tile's frame each atlas cell holds, so a cell is uploaded
    /// only when what it shows changes.
    atlas: [Option<std::path::PathBuf>; recent::SHOWN],
    /// Tile rects from the last launch frame, for hit testing — indexed
    /// like `shown()`.
    hits: Vec<RectPx>,
    hover: Option<usize>,
}

impl RecentRow {
    /// How many candidates get probed for the four tiles.
    const PROBED: usize = recent::SHOWN * 2;

    /// Indices into `tiles` of what the row shows: the first `SHOWN` that
    /// haven't failed. A pending one keeps its place (as an empty well),
    /// so tiles don't reshuffle as workers finish in their own order.
    fn shown(&self) -> Vec<usize> {
        self.tiles.iter().enumerate().filter(|(_, t)| !t.failed).map(|(i, _)| i).take(recent::SHOWN).collect()
    }

    fn pending(&self) -> bool {
        !self.workers.is_empty()
    }

    /// Pick up a new list: keep tiles already probed, spawn workers for
    /// the rest.
    fn refresh(&mut self) {
        self.stale = false;
        let mut old = std::mem::take(&mut self.tiles);
        let mut fresh = Vec::new();
        for p in self.paths.iter().take(Self::PROBED) {
            match old.iter().position(|t| &t.path == p) {
                Some(i) => self.tiles.push(old.swap_remove(i)),
                None => {
                    fresh.push(p.clone());
                    self.tiles.push(RecentTile { path: p.clone(), info: None, rgba: None, failed: false });
                }
            }
        }
        if !fresh.is_empty() {
            self.workers.push(recent::spawn_thumbs(&fresh));
        }
        self.hover = None;
    }

    /// Take whatever the workers finished.
    fn drain(&mut self) {
        let mut done = Vec::new();
        self.workers.retain(|rx| loop {
            match rx.try_recv() {
                Ok(r) => done.push(r),
                Err(std::sync::mpsc::TryRecvError::Empty) => break true,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break false,
            }
        });
        for (path, result) in done {
            let Some(t) = self.tiles.iter_mut().find(|t| t.path == path) else { continue };
            match result {
                Ok(th) => {
                    t.info = Some(th.info);
                    t.rgba = Some(th.rgba);
                }
                Err(e) => {
                    log::info!("recent {}: {e}", path.display());
                    t.failed = true;
                }
            }
        }
    }

    /// Uploads for every atlas cell whose tile changed since it was last
    /// filled.
    fn uploads(&mut self) -> Vec<ThumbUpload> {
        let mut out = Vec::new();
        for (slot, i) in self.shown().into_iter().enumerate() {
            let t = &self.tiles[i];
            let Some(rgba) = &t.rgba else { continue };
            if self.atlas[slot].as_ref() != Some(&t.path) {
                out.push(ThumbUpload { slot, buf: rgba.clone() });
                self.atlas[slot] = Some(t.path.clone());
            }
        }
        out
    }

    fn hit(&self, x: f32, y: f32) -> Option<usize> {
        self.hits.iter().position(|r| contains(*r, x, y))
    }
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
            last_click: None,
            aspect: 0,
            cut: None,
            cut_mode: false,
            cut_drag: None,
            cut_tab: 0,
            cut_thumbs: false,
            cut_last_t: f64::NAN,
            cut_boot: Vec::new(),
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
            video_splash: true,
            backdrop_src: None,
            backdrop: None,
            hidden: false,
            recent: RecentRow::default(),
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

    /// What is under the pointer on the marquee: a white square (corners
    /// win over side squares), otherwise the body, else nothing (the dimmed
    /// area isn't part of the export).
    fn crop_hit(&self, x: f32, y: f32) -> Option<CropGrab> {
        let c = self.crop?;
        let s = self.crop_rect(c);
        for corner in [Corner::Nw, Corner::Ne, Corner::Sw, Corner::Se] {
            let (hx, hy) = corner.of(c);
            let (sx, sy) = self.crop_screen(s, c, hx, hy);
            if (x - sx).abs() <= CROP_GRAB && (y - sy).abs() <= CROP_GRAB {
                return Some(CropGrab::Corner(corner));
            }
        }
        for edge in [Edge::N, Edge::E, Edge::S, Edge::W] {
            let (hx, hy) = edge.of(c);
            let (sx, sy) = self.crop_screen(s, c, hx, hy);
            if (x - sx).abs() <= CROP_GRAB && (y - sy).abs() <= CROP_GRAB {
                return Some(CropGrab::Edge(edge));
            }
        }
        let point = self.image_point(x, y);
        c.contains(point.0, point.1).then_some(CropGrab::Move)
    }

    /// An image-pixel point on the marquee, in screen px.
    fn crop_screen(&self, s: RectPx, c: Crop, hx: f32, hy: f32) -> (f32, f32) {
        (s.x + (hx - c.x) / c.w * s.w, s.y + (hy - c.y) / c.h * s.h)
    }

    /// Take hold of whatever `crop_hit` finds under the press.
    fn crop_grab(&mut self, x: f32, y: f32) {
        let (Some(c), Some(hit)) = (self.crop, self.crop_hit(x, y)) else { return };
        let point = self.image_point(x, y);
        let (hx, hy) = match hit {
            CropGrab::Move => (c.x, c.y),
            CropGrab::Corner(corner) => corner.of(c),
            CropGrab::Edge(edge) => edge.of(c),
        };
        self.crop_drag = Some((hit, (point.0 - hx, point.1 - hy)));
    }

    /// The cursor the marquee wants, or None when the pointer is not on the
    /// canvas with the marquee up (the shell keeps the default arrow).
    pub fn crop_cursor(&self) -> Option<CropCursor> {
        if !self.ready() {
            return (self.cursor_inside && self.recent.hover.is_some()).then_some(CropCursor::Pointer);
        }
        if self.cut_mode && self.show_ui && self.cursor_inside {
            let held = matches!(self.cut_drag, Some(CutDrag::Edge(_)));
            return (held || self.cut_drag.is_none() && self.cut_edge_at(self.cursor.0, self.cursor.1).is_some())
                .then_some(CropCursor::EwResize);
        }
        if !self.mask_mode || self.crop.is_none() || !self.cursor_inside || !self.ready() {
            return None;
        }
        let grab = match self.crop_drag {
            Some((grab, _)) => Some(grab),
            None => {
                let (x, y) = self.cursor;
                if !contains(self.workspace().canvas, x, y) { return None; }
                self.crop_hit(x, y)
            }
        };
        Some(match grab {
            None => CropCursor::Crosshair,
            Some(CropGrab::Move) if self.crop_drag.is_some() => CropCursor::Grabbing,
            Some(CropGrab::Move) => CropCursor::Grab,
            Some(CropGrab::Corner(Corner::Nw | Corner::Se)) => CropCursor::NwseResize,
            Some(CropGrab::Corner(Corner::Ne | Corner::Sw)) => CropCursor::NeswResize,
            Some(CropGrab::Edge(Edge::N | Edge::S)) => CropCursor::NsResize,
            Some(CropGrab::Edge(Edge::E | Edge::W)) => CropCursor::EwResize,
        })
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
            CropGrab::Edge(edge) => match ASPECTS[self.aspect].1 {
                Some(r) => c.with_edge_locked(edge, px - ox, py - oy, r, w, h),
                None => c.with_edge(edge, px - ox, py - oy, w, h),
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
        // Whole pixels, so the four dim rects butt exactly: fractional edges
        // are anti-aliased and the footage shows through the seam as a line.
        let (x1, y1) = ((r.x + r.w).round().clamp(0.0, vw), (r.y + r.h).round().clamp(0.0, vh));
        let (x0, y0) = (r.x.round().clamp(0.0, vw), r.y.round().clamp(0.0, vh));
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
        // Guides: each side's line carries on across the canvas.
        let cv = self.workspace().canvas;
        for y in [r.y, r.y + r.h] {
            if y >= cv.y && y <= cv.y + cv.h {
                items.push(Item::Rect(RectItem::new(
                    RectPx { x: cv.x, y: y - 0.5, w: cv.w, h: 1.0 }, CROP_GUIDE)));
            }
        }
        for x in [r.x, r.x + r.w] {
            if x >= cv.x && x <= cv.x + cv.w {
                items.push(Item::Rect(RectItem::new(
                    RectPx { x: x - 0.5, y: cv.y, w: 1.0, h: cv.h }, CROP_GUIDE)));
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
        self.build_crop_labels(items, c, r);
        let (mx, my) = (r.x + r.w * 0.5, r.y + r.h * 0.5);
        for (hx, hy) in [
            (r.x, r.y), (r.x + r.w, r.y), (r.x, r.y + r.h), (r.x + r.w, r.y + r.h),
            (mx, r.y), (r.x + r.w, my), (mx, r.y + r.h), (r.x, my),
        ] {
            for (size, color) in [(CROP_HANDLE + 2.0, CROP_INK), (CROP_HANDLE, CROP_LINE)] {
                items.push(Item::Rect(RectItem::new(
                    RectPx { x: hx - size * 0.5, y: hy - size * 0.5, w: size, h: size },
                    color,
                )));
            }
        }
    }

    /// The marquee's readouts (the Figma Crop marquee): its size and ratio
    /// in the middle, and the image-pixel coordinates of its top-left and
    /// bottom-right corners pinned inside those corners, each on a flat
    /// chip. The renderer measures every run and sizes its chip, so the
    /// corner labels anchor on the corner itself. A marquee too small to
    /// hold them drops the corners first, then the middle, rather than
    /// letting the labels pile up over the handles.
    fn build_crop_labels(&self, items: &mut Vec<Item>, c: Crop, r: RectPx) {
        let mask = self.masks[self.active].as_ref().unwrap();
        let (x, y, w, h) = c.pixels(mask.width, mask.height);
        let chip = |tx: f32, ty: f32, align: Align, valign: VAlign, text: String| {
            Item::Text(TextItem {
                align,
                valign,
                bg: Some(TextBg { pad_x: CROP_CHIP_PAD.0, pad_y: CROP_CHIP_PAD.1, ..TextBg::new(CROP_CHIP) }),
                ..TextItem::new(tx, ty, 10.0, WORKSPACE_TEXT, text)
            })
        };
        let (px, py) = CROP_CHIP_PAD;
        if r.w >= CROP_LABELS_MIN.0 && r.h >= CROP_LABELS_MIN.1 {
            items.push(chip(r.x + px, r.y + py, Align::Left, VAlign::Top, format!("{x}, {y}")));
            items.push(chip(r.x + r.w - px, r.y + r.h - py, Align::Right, VAlign::Bottom,
                format!("{}, {}", x + w, y + h)));
        }
        if r.w >= CROP_LABEL_MIN.0 && r.h >= CROP_LABEL_MIN.1 {
            let (name, locked) = ASPECTS[self.aspect];
            let ratio = ratio_label(w, h, locked.map(|_| name));
            items.push(chip(r.x + r.w / 2.0, r.y + r.h / 2.0, Align::Center, VAlign::Middle,
                format!("{w}x{h} - {ratio}")));
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

    /// The recent-files list (`recent::load`), most recent first. The
    /// launch window re-reads it the next time it draws.
    pub fn set_recent(&mut self, paths: Vec<std::path::PathBuf>) {
        self.recent.paths = paths;
        self.recent.stale = true;
    }

    /// The file behind the launch window's nth recent tile.
    pub fn recent_path(&self, n: usize) -> Option<std::path::PathBuf> {
        let i = *self.recent.shown().get(n)?;
        Some(self.recent.tiles[i].path.clone())
    }

    fn open_recent(&mut self, n: usize) {
        if n < self.recent.shown().len() {
            self.cmds.push(Cmd::OpenRecent(n));
        }
    }

    /// The video the launch window plays as its floor (probed by main).
    pub fn set_backdrop(&mut self, info: VideoInfo) {
        self.backdrop_src = Some(info);
    }

    /// Occluded windows don't decode the backdrop (see `hidden`).
    pub fn set_hidden(&mut self, hidden: bool) {
        self.hidden = hidden;
    }

    /// Whether the launch window animates its plate. Off (any command-line
    /// argument) leaves the bare centered mark and no backdrop decoder.
    pub fn set_video_splash(&mut self, enabled: bool) {
        self.video_splash = enabled;
        if !enabled {
            self.backdrop = None;
            self.backdrop_src = None;
        }
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
        self.cut = None;
        self.cut_mode = false;
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
        self.cut = None;
        self.cut_mode = false;
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

    pub fn key(&mut self, mut k: Key) {
        if let Key::CmdDigit(c) = k {
            if self.ready() {
                k = Key::Char(c);
            } else {
                if let Some(d) = c.to_digit(10).filter(|d| *d >= 1) {
                    self.open_recent(d as usize - 1);
                }
                return;
            }
        }
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
        if !self.cut_mode {
            k = match k { Key::KeyLeft => Key::Left, Key::KeyRight => Key::Right, k => k };
        }
        if !self.mask_mode && matches!(k, Key::Char('t' | 'T')) {
            self.set_cut_mode(!self.cut_mode);
            return;
        }
        if k == Key::Char('m') || k == Key::Char('M') {
            self.set_cut_mode(false);
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
        if self.cut_mode && self.cut_key(k) {
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
            Key::Close | Key::CmdDigit(_) | Key::Undo | Key::KeyLeft | Key::KeyRight => {} // handled above
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
        if !self.ready() {
            self.cursor = (x, y);
            self.recent.hover = self.recent.hit(x, y);
            return;
        }
        if self.scrubbing {
            self.scrub_to(x);
            self.cursor = (x, y);
            return;
        }
        if self.cut_drag.is_some() {
            self.cut_drag_to(x);
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
        if !self.ready() {
            // The launch window: a recent tile opens its file; anywhere
            // else is just the drop target.
            if let Some(n) = self.recent.hit(x, y) { self.open_recent(n); }
            return;
        }
        self.cursor = (x, y);
        self.cursor_inside = true;
        if self.show_ui {
            if let Some(control) = self.controls().into_iter().find(|c| contains(c.r, x, y)) {
                self.activate(control.action);
                return;
            }
            if !self.cut_mode && contains(self.workspace().list, x, y) {
                let first = self.source_first();
                for idx in first..(first + self.source_capacity()).min(self.videos.len()) {
                    if contains(self.source_row(idx), x, y) { self.select(idx); break; }
                }
                return;
            }
            if !self.cut_mode {
                if contains(self.btn_prev(), x, y) { self.step(-1); return; }
                if contains(self.btn_play(), x, y) { self.playing = !self.playing; return; }
                if contains(self.btn_next(), x, y) { self.step(1); return; }
            }
            if self.cut_mode {
                if self.cut_press(x, y) { return; }
            }
            let s = self.seek_rect(self.vp);
            if !self.cut_mode && contains(RectPx { x: s.x - 6.0, y: s.y - SEEK_GRAB, w: s.w + 12.0, h: s.h + 2.0 * SEEK_GRAB }, x, y) {
                self.scrubbing = true;
                self.scrub_to(x);
                return;
            }
        }
        if !contains(self.workspace().canvas, x, y) { return; }
        if self.mask_mode {
            self.stroke_last = None;
            if self.crop.is_some() {
                // Double-click inside the marquee: back out to the whole frame.
                let now = std::time::Instant::now();
                let double = self.last_click.is_some_and(|(t, (lx, ly))| {
                    now - t < DOUBLE_CLICK && (x - lx).abs() < 4.0 && (y - ly).abs() < 4.0
                });
                self.last_click = Some((now, (x, y)));
                if double && matches!(self.crop_hit(x, y), Some(CropGrab::Move)) {
                    self.crop = Some(self.full_crop());
                    self.crop_drag = None;
                    self.last_click = None;
                    self.mask_status.clear();
                    return;
                }
                self.crop_grab(x, y);
                return;
            }
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
        self.cut_drag = None;
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
        if self.cut_mode && self.show_ui && contains(self.workspace().timeline, self.cursor.0, self.cursor.1) {
            let lane = self.cut_lanes().video;
            if dy.abs() > dx.abs() {
                // Up and down zoom about the pointer: scrolling up dives in
                // (the content under the pointer stays put), down backs out.
                let anchor = (self.cursor.0 - lane.x).clamp(0.0, lane.w);
                if let Some(cut) = &mut self.cut { cut.zoom((-(dy as f64) * 0.012).exp(), anchor, lane.w); }
            } else {
                // Sideways: fingers move the content, right = earlier.
                if let Some(cut) = &mut self.cut { cut.pan(-(dx as f64) / cut.pps.max(1e-6), lane.w); }
            }
            return;
        }
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
        if self.ready() && self.cut_mode && self.show_ui && contains(self.workspace().timeline, self.cursor.0, self.cursor.1) {
            let lane = self.cut_lanes().video;
            let anchor = (self.cursor.0 - lane.x).clamp(0.0, lane.w);
            // Exponential, a touch quicker than the video's: spreading two fingers
            // over the track dives in about the pointer, pinching closes back out.
            if let Some(cut) = &mut self.cut { cut.zoom((delta as f64 * 1.8).exp(), anchor, lane.w); }
            return;
        }
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
        if self.shown_mode() == Mode::SideBySide && !self.single() {
            cell.w /= self.videos.len().max(1) as f32;
            cell.x += idx as f32 * cell.w;
        }
        cell
    }

    fn gesture_base(&self, x: f32, _y: f32) -> RectPx {
        let idx = if self.shown_mode() == Mode::SideBySide && !self.single() {
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
        if let Some(cut) = &mut self.cut { cut.poll(); }
        if !self.cut_boot.is_empty() { self.boot_cut(); }
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
            // The logo-only window (any command-line argument) draws no
            // recent row, so it spawns no thumbnail workers either.
            if self.video_splash {
                if self.recent.stale {
                    self.recent.refresh();
                }
                self.recent.drain();
            }
            let mut desc = self.launch_frame(vp);
            desc.thumbs = self.recent.uploads();
            // The floor moves at the clip's own rate: wake for its next
            // frame instead of running the loop hot at the display's.
            if let Some(b) = &self.backdrop_src {
                desc.redraw_at =
                    Some(Instant::now() + std::time::Duration::from_secs_f64(1.0 / b.fps.max(1.0)));
            }
            // Thumbnail workers still out: look again soon even without a
            // moving floor to pace the loop.
            if self.recent.pending() {
                let soon = Instant::now() + std::time::Duration::from_millis(100);
                desc.redraw_at = Some(desc.redraw_at.map_or(soon, |t| t.min(soon)));
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
            // Cut mode plays the RESULT: removed material is jumped over.
            if self.cut_mode && let Some(to) = self.cut.as_ref().and_then(|c| c.skip(self.t)) {
                let end = if self.wrap.is_finite() { self.wrap - 0.05 } else { f64::MAX };
                self.seek_all(if to >= end { 0.0 } else { to }, true);
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
        if self.cut_mode {
            let width = self.cut_lanes().video.w;
            let (t, moved, held) = (self.t, self.t != self.cut_last_t, self.cut_drag.is_some());
            if let Some(cut) = &mut self.cut {
                cut.fit_view(width);
                if moved && !held { cut.reveal(t, width); }
            }
            self.cut_last_t = self.t;
        }
        let mode = if self.single() { Mode::Overlay } else { self.shown_mode() };
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
        if self.cut_mode && self.show_ui { self.build_cut_chips(&mut items); }
        items.push(Item::Clip(None));
        if !self.cut_mode && (self.show_ui || self.badge_flash > 0.0) {
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
            if self.cut_mode { self.build_cut_hud(&mut items, vp); } else { self.build_hud(&mut items, vp); }
            self.build_status_line(&mut items, vp);
        }

        let animating =
            self.playing || self.badge_flash > 0.0 || !self.started;
        FrameDesc {
            clear: FRAME_BG,
            uploads,
            plate: None,
            thumbs: Vec::new(),
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
        Workspace::new(self.vp, self.show_ui, self.cut_mode)
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
        let tool = if self.cut_mode { 3 } else if !self.mask_mode { 0 } else if self.crop.is_none() { 1 } else { 2 };
        let mut x = if self.fullscreen { 16.0 } else { 100.0 };
        for (i, (label, w)) in [("INPUT", 62.0), ("MASK", 56.0), ("CROP", 56.0), ("CUT", 48.0)].into_iter().enumerate() {
            out.push(Control::new(RectPx { x, y: 6.0, w, h: 26.0 }, label, tool == i, Action::Tool(i)));
            x += w + 4.0;
        }
        if self.cut_mode {
            self.cut_controls(&mut out, x);
            return out;
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
                self.set_cut_mode(false);
                if self.mask_mode { self.mask_mode = false; self.leave_mask_mode(); }
            }
            Action::Tool(3) => self.set_cut_mode(true),
            Action::Cut(action) => self.cut_action(action),
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

    /// Step back, play / pause, step on: shared by Input's transport and
    /// cut mode's.
    fn build_transport_buttons(&self, items: &mut Vec<Item>) {
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
    }

    fn build_transport(&self, items: &mut Vec<Item>) {
        let l = self.workspace();
        self.build_transport_buttons(items);
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
        if self.cut_mode {
            let cut = self.cut.as_ref().unwrap();
            ui_label(items, 16.0, y + STATUS_H / 2.0, 10.0, ACCENT, "CUT", Align::Left, 70.0);
            let status = if !cut.status.is_empty() { cut.status.clone() } else if cut.scanning() { "Scanning keyframes…".into() } else {
                format!("{} · {} cut · snap to {}", name_of(&cut.path), cut.cuts().len(),
                    if cut.snap == Snap::Keyframe { "keyframes, stream copy" } else { "frames, re-encode" })
            };
            let hint_width = if vp.0 >= 1000.0 { 440.0 } else { 0.0 };
            ui_label(items, 98.0, y + STATUS_H / 2.0, 10.0, WORKSPACE_DIM, status, Align::Left, vp.0 - 118.0 - hint_width);
            if hint_width > 0.0 {
                ui_label(items, vp.0 - 16.0, y + STATUS_H / 2.0, 10.0, WORKSPACE_MUTED,
                    "i in   o out   s split   x cut   u undo   k snap   e export   esc back", Align::Right, hint_width);
            }
            return;
        }
        let mode = if !self.mask_mode { "INPUT" } else if self.crop.is_some() { "CROP" } else { "MASK" };
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
    fn launch_frame(&mut self, vp: (f32, f32)) -> FrameDesc {
        let (w, h) = vp;
        let mut items: Vec<Item> = Vec::new();

        if !self.video_splash {
            let lw = (w * 0.34).clamp(240.0, 460.0).min((w - 96.0).max(1.0));
            let lh = lw / self.logo_aspect;
            return FrameDesc {
                clear: LAUNCH_BG,
                uploads: Vec::new(),
                thumbs: Vec::new(),
                plate: None,
                items: vec![Item::Logo {
                    r: RectPx { x: (w - lw) / 2.0, y: (h - lh) / 2.0, w: lw, h: lh },
                    alpha: 1.0,
                }, build_label(w, h)],
                animating: false,
                redraw_at: None,
            };
        }

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
        items.push(build_label(w, h));

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
        // The recent row lies on the floor between the mark and the
        // foot; only the plate has the room for it.
        self.recent.hits.clear();
        if splash {
            self.push_recent_row(&mut items, w, horizon + RECENT_CLEAR, foot - RECENT_CLEAR);
        }
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
            thumbs: Vec::new(),
            items,
            animating: false,
            redraw_at: None,
        }
    }
}

impl App {
    /// The launch window's recent files: a header (`RECENT`, `⌘1–n`) and
    /// one tile per file — a rounded, bevelled thumbnail with the
    /// container at its top-left and the running time at its
    /// bottom-right, and the frame size and rate underneath. Centred in
    /// the band `top..bottom`; left out when the band is too short or
    /// there is nothing to show. Records each tile's rect for hit testing.
    fn push_recent_row(&mut self, items: &mut Vec<Item>, w: f32, top: f32, bottom: f32) {
        let shown = self.recent.shown();
        let n = shown.len();
        let row_h = RECENT_HEAD + RECENT_STEP + TILE_H + RECENT_STEP + RECENT_META;
        if n == 0 || bottom - top < row_h {
            return;
        }
        let row_w = n as f32 * TILE_W + (n as f32 - 1.0) * TILE_GAP;
        if row_w > w - LOCKUP_CLEAR * 2.0 {
            return;
        }
        let x0 = (w - row_w) / 2.0;
        let y0 = top + ((bottom - top) - row_h) / 2.0;

        let head = TextItem {
            valign: VAlign::Middle,
            tracking: 10.0 * 0.09,
            ..TextItem::new(x0, y0 + RECENT_HEAD / 2.0, 10.0, INK_MUTED, "RECENT")
        };
        items.push(Item::Text(head));
        let keys = if n == 1 { "⌘1".to_string() } else { format!("⌘1–{n}") };
        items.push(Item::Text(TextItem {
            align: Align::Right,
            valign: VAlign::Middle,
            ..TextItem::new(x0 + row_w, y0 + RECENT_HEAD / 2.0, 10.0, INK_FAINT, keys)
        }));

        let ty = y0 + RECENT_HEAD + RECENT_STEP;
        let chip = TextBg { radius: 4.0, pad_x: 4.0, pad_y: 1.0, ..TextBg::new(CHIP_BG) };
        for (slot, &i) in shown.iter().enumerate() {
            let t = &self.recent.tiles[i];
            let r = RectPx { x: x0 + slot as f32 * (TILE_W + TILE_GAP), y: ty, w: TILE_W, h: TILE_H };
            let hot = self.recent.hover == Some(slot);
            // The recessed well first; the frame sits in it one px in, so
            // the bevel's hairline runs round the picture, not over it.
            items.push(Item::Rect(RectItem { radius: TILE_R, ..RectItem::new(r, WELL) }));
            if t.rgba.is_some() {
                let inner = RectPx { x: r.x + 1.0, y: r.y + 1.0, w: r.w - 2.0, h: r.h - 2.0 };
                items.push(Item::Thumb { r: inner, slot, radius: TILE_R - 1.0, alpha: 1.0 });
            }
            // The bevel: a faint white outline all round, and a brighter
            // one that is strongest along the top edge and gone by the
            // bottom — lit from above, as the moulded controls are.
            items.push(Item::Rect(RectItem {
                radius: TILE_R,
                border_w: 1.0,
                border_color: if hot { BRAND_BLUE } else { BEVEL },
                ..RectItem::new(r, [0.0; 4])
            }));
            items.push(Item::Rect(RectItem {
                radius: TILE_R,
                border_w: 1.0,
                border_color: BEVEL_LIT,
                fade_down: true,
                ..RectItem::new(r, [0.0; 4])
            }));
            if let Some(info) = &t.info {
                items.push(Item::Text(TextItem {
                    bg: Some(chip),
                    ..TextItem::new(r.x + 9.0, r.y + 6.0, 10.0, CHIP_INK, recent::container(&t.path))
                }));
                if info.duration > 0.0 {
                    items.push(Item::Text(TextItem {
                        align: Align::Right,
                        valign: VAlign::Bottom,
                        bg: Some(chip),
                        ..TextItem::new(r.x + r.w - 9.0, r.y + r.h - 6.0, 10.0, CHIP_INK, recent::fmt_duration(info.duration))
                    }));
                }
                let my = r.y + r.h + RECENT_STEP + RECENT_META / 2.0;
                items.push(Item::Text(TextItem {
                    valign: VAlign::Middle,
                    max_width: Some(TILE_W / 2.0),
                    ..TextItem::new(r.x, my, 11.0, INK_MUTED, format!("{}×{}", info.width, info.height))
                }));
                items.push(Item::Text(TextItem {
                    align: Align::Right,
                    valign: VAlign::Middle,
                    max_width: Some(TILE_W / 2.0),
                    ..TextItem::new(r.x + r.w, my, 11.0, INK_MUTED, format!("{} fps", recent::fmt_fps(info.fps)))
                }));
            }
            self.recent.hits.push(r);
        }
    }
}

/// Cut mode's lane rectangles, shared by drawing and hit testing. Every
/// lane has the same x and width: one time axis.
#[derive(Clone, Copy)]
struct CutLanes {
    ruler: RectPx,
    chapters: Option<RectPx>,
    video: RectPx,
    audio: Option<RectPx>,
    subs: RectPx,
    keys: RectPx,
}

/// The inspector's rectangles: see `App::cut_side`.
struct CutSide {
    tabs: RectPx,
    tab: [RectPx; 2],
    chips_y: f32,
    count_y: f32,
    toggle: RectPx,
    add_btn: RectPx,
    list_btn: RectPx,
    grid_btn: RectPx,
    footer_y: f32,
    warning: Vec<String>,
    warning_y: f32,
    warning_h: f32,
    body: RectPx,
}

/// Cut mode: the timeline edit of one clip (`cut.rs` is the model). This is
/// the "Cut" board of the Abner Timeline Edit design — same shell as
/// Input, with the source rail traded for an inspector on the right and
/// the lanes panel under the transport.
impl App {
    /// Mask and cut both show the focused clip alone, whatever the view.
    fn single(&self) -> bool {
        self.mask_mode || self.cut_mode
    }

    /// `--cut [in,out[,in,out…]]`: open on the timeline, for targeted
    /// captures. Every pair but the last is cut; the last is left as the
    /// selection. Held until the keyframe scan lands (`tick`), so the
    /// ranges snap exactly as they would by hand.
    pub fn start_cut(&mut self, ranges: Vec<f64>) {
        self.set_cut_mode(true);
        if self.cut_mode { self.cut_boot = ranges; }
    }

    fn boot_cut(&mut self) {
        let Some(cut) = self.cut.as_mut().filter(|c| !c.scanning()) else { return };
        let ranges = std::mem::take(&mut self.cut_boot);
        let pairs = ranges.len() / 2;
        for (i, pair) in ranges.chunks_exact(2).enumerate() {
            cut.set_in(pair[0]);
            cut.set_out(pair[1]);
            if i + 1 < pairs { cut.cut_selection(); }
        }
        if let Some(at) = cut.in_req {
            self.playing = false;
            self.seek_all(at, true);
        }
    }

    fn set_cut_mode(&mut self, on: bool) {
        if on == self.cut_mode || !self.ready() {
            return;
        }
        self.mouse_up();
        if on {
            let info = &self.videos[self.active].info;
            if info.duration <= 0.1 {
                log::error!("cut: {} reports no duration", info.path.display());
                return;
            }
            if self.mask_mode {
                self.mask_mode = false;
                self.leave_mask_mode();
            }
            let info = &self.videos[self.active].info;
            if self.cut.as_ref().is_none_or(|c| c.path != info.path) {
                self.cut = Some(Cut::open(&info.path, info.duration));
            }
            self.zoom = 1.0;
            self.center = (0.5, 0.5);
            self.cut_last_t = f64::NAN;
        }
        self.cut_mode = on;
    }

    /// Cut mode's own keys. False = not one of them, fall through.
    fn cut_key(&mut self, k: Key) -> bool {
        let t = self.t;
        let action = match k {
            Key::Char('i' | 'I') => CutAction::In,
            Key::Char('o' | 'O') => CutAction::Out,
            Key::Char('s' | 'S') => CutAction::Split,
            Key::Char('x' | 'X') => CutAction::Apply,
            Key::Char('e' | 'E') => CutAction::Export,
            Key::Undo | Key::Char('u' | 'U') => CutAction::Undo,
            Key::Char('k' | 'K') => {
                let snap = self.cut.as_ref().map(|c| c.snap);
                CutAction::Snap(if snap == Some(Snap::Keyframe) { Snap::Frame } else { Snap::Keyframe })
            }
            // The design's arrows: a frame, and with shift a keyframe.
            Key::Left => { self.step(-1); return true; }
            Key::Right => { self.step(1); return true; }
            Key::KeyLeft | Key::KeyRight => {
                let dir = if k == Key::KeyRight { 1 } else { -1 };
                let to = self.cut.as_ref().map(|c| c.key_step(t, dir)).unwrap_or(t);
                let end = if self.wrap.is_finite() { (self.wrap - 0.05).max(0.0) } else { f64::MAX };
                self.playing = false;
                self.seek_all(to.min(end), true);
                return true;
            }
            Key::Escape => {
                let cut = self.cut.as_mut().unwrap();
                if cut.in_req.is_some() || cut.out_req.is_some() {
                    cut.clear_selection();
                } else {
                    self.set_cut_mode(false);
                }
                return true;
            }
            // One clip: nothing to flip to, no compare view to change.
            Key::Enter | Key::Char('1'..='9' | 'v' | 'V' | '-' | '=' | '+') => return true,
            _ => return false,
        };
        self.cut_action(action);
        true
    }

    fn cut_action(&mut self, action: CutAction) {
        let t = self.t;
        // S cuts where the razor shows, when it shows.
        let split_at = self.cut_razor().unwrap_or(t);
        let lane = self.cut_lanes().video;
        if action == CutAction::Play {
            self.playing = !self.playing;
            return;
        }
        // The skip buttons move the playhead; they pause, as the arrow keys do.
        if let CutAction::Step(dir) = action {
            self.step(dir);
            return;
        }
        if let CutAction::Keyframe(_) | CutAction::Chapter(_) = action {
            let Some(cut) = self.cut.as_ref() else { return };
            let to = match action {
                CutAction::Keyframe(dir) => Some(cut.key_step(t, dir)),
                CutAction::Chapter(dir) => cut.chapter_step(t, dir),
                _ => None,
            };
            if let Some(to) = to {
                let end = if self.wrap.is_finite() { (self.wrap - 0.05).max(0.0) } else { f64::MAX };
                self.playing = false;
                self.seek_all(to.min(end), true);
            }
            return;
        }
        let Some(cut) = self.cut.as_mut() else { return };
        cut.status.clear();
        match action {
            CutAction::Step(_) | CutAction::Keyframe(_) | CutAction::Chapter(_) => {}
            CutAction::Play => {}
            CutAction::Zoom(zoom_in) => {
                let px = ((t - cut.t0) * cut.pps) as f32;
                cut.zoom(if zoom_in { 1.5 } else { 1.0 / 1.5 }, if px >= 0.0 && px <= lane.w { px } else { lane.w / 2.0 }, lane.w);
            }
            CutAction::In => cut.set_in(t),
            CutAction::Out => cut.set_out(t),
            CutAction::Split => {
                if !cut.split(split_at) { cut.status = "No split here: already a boundary, or inside a cut".into(); }
            }
            CutAction::Apply => {
                let restoring = cut.selection_is_cut();
                match cut.selection() {
                    Some((a, b)) => {
                        cut.cut_selection();
                        cut.status = format!("{} {:.2} s", if restoring { "Restored" } else { "Cut" }, b - a);
                    }
                    None => cut.status = "Set in (I) and out (O) first".into(),
                }
            }
            CutAction::Undo => {
                if !cut.undo() { cut.status = "Nothing to undo".into(); }
            }
            CutAction::Export => cut.start_export(),
            CutAction::Snap(snap) => cut.snap = snap,
        }
    }

    fn cut_lanes(&self) -> CutLanes {
        let tl = self.workspace().timeline;
        let x = tl.x + CUT_GUTTER;
        let w = (tl.w - CUT_GUTTER - CUT_PAD).max(1.0);
        let lane = |top: f32, h: f32| RectPx { x, y: tl.y + top, w, h };
        // The board's own offsets; the compact panel closes up the rows
        // it drops.
        if tl.h >= CUT_TIMELINE_H {
            CutLanes { ruler: lane(10.0, 18.0), chapters: Some(lane(40.0, 24.0)), video: lane(68.0, 56.0),
                audio: Some(lane(128.0, 52.0)), subs: lane(184.0, 12.0), keys: lane(200.0, 18.0) }
        } else {
            CutLanes { ruler: lane(10.0, 18.0), chapters: None, video: lane(36.0, 56.0),
                audio: None, subs: lane(96.0, 12.0), keys: lane(112.0, 18.0) }
        }
    }

    /// The header's Undo / Export and the scrub toolbar's icon buttons.
    /// `label` is what the button says when hovered; `hint` its key.
    fn cut_controls(&self, out: &mut Vec<Control>, tabs_end: f32) {
        let Some(cut) = &self.cut else { return };
        let (w, bar) = (self.vp.0, self.workspace().transport);
        let mut push = |r: RectPx, label: &str, selected: bool, action: CutAction, hint: &'static str| {
            out.push(Control { hint, ..Control::new(r, label, selected, Action::Cut(action)) });
        };
        if w - 180.0 > tabs_end {
            push(RectPx { x: w - 16.0 - 64.0, y: 6.0, w: 64.0, h: 26.0 }, "Export", false, CutAction::Export, "");
            push(RectPx { x: w - 16.0 - 64.0 - 8.0 - 88.0, y: 6.0, w: 88.0, h: 26.0 }, "Undo", false, CutAction::Undo, "⌘Z");
        }
        let cy = bar.y + bar.h / 2.0;
        // Either side of play, outward to in: a chapter, a keyframe, a frame.
        let x0 = bar.x + 16.0;
        for (i, (label, action, hint)) in [
            ("Previous chapter", CutAction::Chapter(-1), ""), ("Previous keyframe", CutAction::Keyframe(-1), "⇧←"),
            ("Back one frame", CutAction::Step(-1), "←"),
        ].into_iter().enumerate() {
            push(RectPx { x: x0 + i as f32 * 32.0, y: cy - 14.0, w: 28.0, h: 28.0 }, label, false, action, hint);
        }
        push(RectPx { x: x0 + 100.0, y: cy - 15.0, w: 30.0, h: 30.0 }, if self.playing { "Pause" } else { "Play" }, false, CutAction::Play, "space");
        for (i, (label, action, hint)) in [
            ("Forward one frame", CutAction::Step(1), "→"), ("Next keyframe", CutAction::Keyframe(1), "⇧→"),
            ("Next chapter", CutAction::Chapter(1), ""),
        ].into_iter().enumerate() {
            push(RectPx { x: x0 + 138.0 + i as f32 * 32.0, y: cy - 14.0, w: 28.0, h: 28.0 }, label, false, action, hint);
        }
        let mut x = self.cut_tools_x();
        let keyframe = cut.snap == Snap::Keyframe;
        for (label, action, hint) in [
            ("Set in", CutAction::In, "I"), ("Set out", CutAction::Out, "O"), ("Split", CutAction::Split, "S"),
            (if cut.selection_is_cut() { "Restore selection" } else { "Cut selection" }, CutAction::Apply, "X"),
            (if keyframe { "Snap to keyframes: on" } else { "Snap to keyframes: off" },
                CutAction::Snap(if keyframe { Snap::Frame } else { Snap::Keyframe }), "K"),
        ] {
            push(RectPx { x, y: cy - 16.0, w: 32.0, h: 32.0 }, label, matches!(action, CutAction::Snap(_)) && keyframe, action, hint);
            x += 42.0;
        }
        let zoom = self.cut_zoom_bar();
        push(RectPx { x: zoom.x - 30.0, y: cy - 12.0, w: 24.0, h: 24.0 }, "Zoom out", false, CutAction::Zoom(false), "");
        push(RectPx { x: zoom.x + zoom.w + 6.0, y: cy - 12.0, w: 24.0, h: 24.0 }, "Zoom in", false, CutAction::Zoom(true), "");
    }

    /// Where the toolbar's icon buttons start: after the play disc, the
    /// timecode and the separator.
    fn cut_tools_x(&self) -> f32 {
        self.cut_clock_x() + 11.0 * 20.0 * MONO_ADV + 33.0
    }

    /// The timecode's left edge: past the skip buttons and the play disc.
    fn cut_clock_x(&self) -> f32 {
        self.workspace().transport.x + 16.0 + 246.0
    }

    /// The zoom slider's track, at the toolbar's right.
    fn cut_zoom_bar(&self) -> RectPx {
        let bar = self.workspace().transport;
        RectPx { x: bar.x + bar.w - 16.0 - 30.0 - 110.0, y: bar.y + bar.h / 2.0 - 1.5, w: 110.0, h: 3.0 }
    }

    /// The source time under a pointer x on the lanes.
    fn cut_time_at(&self, x: f32) -> f64 {
        let lane = self.cut_lanes().video;
        self.cut.as_ref().map_or(0.0, |c| c.t0 + ((x - lane.x) as f64) / c.pps.max(1e-6))
    }

    /// The clip edge under the pointer, if it is on one's grip: the video
    /// and audio rows, within a few px of a boundary that is on screen.
    fn cut_edge_at(&self, x: f32, y: f32) -> Option<f64> {
        let lanes = self.cut_lanes();
        let cut = self.cut.as_ref()?;
        let v = lanes.video;
        let bottom = lanes.audio.map_or(v.y + v.h, |a| a.y + a.h);
        if y < v.y || y >= bottom || x < v.x - CUT_GRIP || x > v.x + v.w + CUT_GRIP { return None; }
        let t = self.cut_time_at(x);
        let reach = CUT_GRIP as f64 / cut.pps.max(1e-6);
        cut.edges().into_iter().filter(|e| (e - t).abs() <= reach)
            .min_by(|a, b| (a - t).abs().total_cmp(&(b - t).abs()))
    }

    /// Where `S` would split right now: under the pointer while it is over
    /// the clips (the razor line shows it), at the playhead otherwise.
    fn cut_razor(&self) -> Option<f64> {
        let (x, y) = self.cursor;
        if !self.show_ui || !self.cursor_inside || self.cut_drag.is_some() { return None; }
        let lanes = self.cut_lanes();
        let v = lanes.video;
        let bottom = lanes.audio.map_or(v.y + v.h, |a| a.y + a.h);
        if x < v.x || x >= v.x + v.w || y < v.y || y >= bottom || self.cut_edge_at(x, y).is_some() { return None; }
        let cut = self.cut.as_ref()?;
        let t = cut.split_point(self.cut_time_at(x));
        (t > cut.t0 && !cut.cuts().iter().any(|c| t > c.0 && t < c.1)).then_some(t)
    }

    /// A press below the header in cut mode. True = it was the scrub
    /// panel's or the inspector's, whatever it hit there.
    fn cut_press(&mut self, x: f32, y: f32) -> bool {
        let l = self.workspace();
        if contains(l.transport, x, y) {
            let zoom = self.cut_zoom_bar();
            if contains(RectPx { x: zoom.x - 4.0, y: zoom.y - 10.0, w: zoom.w + 8.0, h: zoom.h + 20.0 }, x, y) {
                self.cut_drag = Some(CutDrag::Zoom);
                self.cut_drag_to(x);
            }
            return true;
        }
        if contains(l.timeline, x, y) {
            if let Some(edge) = self.cut_edge_at(x, y) {
                if let Some(cut) = &mut self.cut { cut.begin_trim(); }
                self.playing = false;
                self.cut_drag = Some(CutDrag::Edge(edge));
            } else if let Some(seg) = self.cut_ghost_at(x, y) {
                // A removed clip is a selection: X restores it.
                let cut = self.cut.as_mut().unwrap();
                cut.in_req = Some(seg.start);
                cut.out_req = Some(seg.end);
                cut.status.clear();
            } else if x >= self.cut_lanes().video.x - 4.0 {
                self.cut_drag = Some(CutDrag::Scrub);
                self.cut_drag_to(x);
            }
            return true;
        }
        if contains(l.side, x, y) {
            let side = self.cut_side();
            if let Some(i) = side.tab.iter().position(|r| contains(*r, x, y)) {
                self.cut_tab = i;
            } else if self.cut_tab == 0 && contains(side.add_btn, x, y) {
                // A chapter starts here: the playhead.
                let t = self.t;
                if let Some(cut) = self.cut.as_mut() {
                    cut.status = match cut.add_chapter(t) {
                        Some(n) => format!("Chapter {n} added at {}", fmt_hms(t)),
                        None => "A chapter already starts here".into(),
                    };
                }
            } else if self.cut_tab == 0 && contains(side.list_btn, x, y) {
                self.cut_thumbs = false;
            } else if self.cut_tab == 0 && contains(side.grid_btn, x, y) {
                self.cut_thumbs = true;
            } else if let Some((_, idx)) = self.cut_chapter_rows().into_iter().find(|(r, _)| contains(*r, x, y)) {
                // A chapter is a place: go there.
                let start = self.cut.as_ref().unwrap().chapters[idx].start;
                let end = if self.wrap.is_finite() { (self.wrap - 0.05).max(0.0) } else { f64::MAX };
                self.seek_all(start.min(end), true);
            }
            return true;
        }
        false
    }

    /// The removed clip under the pointer on the picture or sound row.
    fn cut_ghost_at(&self, x: f32, y: f32) -> Option<crate::cut::Segment> {
        let lanes = self.cut_lanes();
        let rows = [Some(lanes.video), lanes.audio].into_iter().flatten();
        if !rows.into_iter().any(|r| y >= r.y && y < r.y + r.h) { return None; }
        let at = self.cut_time_at(x);
        self.cut.as_ref()?.segments().into_iter().find(|g| g.cut && at >= g.start && at < g.end)
    }

    fn cut_drag_to(&mut self, x: f32) {
        let lane = self.cut_lanes().video;
        let zoom = self.cut_zoom_bar();
        let at = self.cut_time_at(x);
        let playhead = self.t;
        let Some(cut) = self.cut.as_mut() else { return };
        let end = (cut.duration.min(self.wrap) - 0.05).max(0.0);
        match self.cut_drag {
            Some(CutDrag::Scrub) => self.seek_all(at.clamp(0.0, end), true),
            Some(CutDrag::Edge(edge)) => {
                // The canvas follows the edge, so the trim is made by eye.
                let now = cut.move_edge(edge, at);
                self.cut_drag = Some(CutDrag::Edge(now));
                self.seek_all(now.min(end), true);
            }
            Some(CutDrag::Zoom) => {
                // About the playhead when it is on screen, the middle if not.
                let px = ((playhead - cut.t0) * cut.pps) as f32;
                let anchor = if px >= 0.0 && px <= lane.w { px } else { lane.w / 2.0 };
                cut.set_zoom_fraction(((x - zoom.x) / zoom.w).clamp(0.0, 1.0) as f64, anchor, lane.w);
            }
            None => {}
        }
    }

    /// The inspector's geometry, shared by drawing and hit testing: icon
    /// tabs, the chip strip, the count row with the list / thumbnails
    /// toggle, the body, and a footer holding the result.
    fn cut_side(&self) -> CutSide {
        let s = self.workspace().side;
        let (x0, w) = (s.x + 16.0, s.w - 32.0);
        let tabs = RectPx { x: x0, y: s.y + 16.0, w, h: 32.0 };
        let half = (w - 4.0) / 2.0;
        let tab = [0, 1].map(|i| RectPx { x: x0 + 2.0 + i as f32 * half, y: tabs.y + 2.0, w: half, h: 28.0 });
        let chips_y = tabs.y + tabs.h + 14.0;
        let count_y = if self.cut_tab == 0 { chips_y + 34.0 } else { tabs.y + tabs.h + 12.0 };
        let toggle = RectPx { x: x0 + w - 56.0, y: count_y, w: 56.0, h: 24.0 };
        let add_btn = RectPx { x: toggle.x - 8.0 - 26.0, y: toggle.y + 1.0, w: 26.0, h: 22.0 };
        let list_btn = RectPx { x: toggle.x + 2.0, y: toggle.y + 2.0, w: 26.0, h: 20.0 };
        let grid_btn = RectPx { x: toggle.x + 28.0, ..list_btn };
        let footer_y = s.y + s.h - 56.0;
        let warning = self.cut.as_ref().and_then(|c| c.warning()).map(|w| wrap_text(&w, 38)).unwrap_or_default();
        let warning_h = if warning.is_empty() { 0.0 } else { 16.0 + warning.len() as f32 * 15.0 };
        let warning_y = footer_y - 10.0 - warning_h;
        let body_top = if self.cut_tab == 0 { count_y + 24.0 + 10.0 } else { tabs.y + tabs.h + 16.0 };
        let body_bottom = if warning.is_empty() { footer_y - 10.0 } else { warning_y - 8.0 };
        CutSide { tabs, tab, chips_y, count_y, toggle, add_btn, list_btn, grid_btn, footer_y, warning, warning_y, warning_h,
            body: RectPx { x: x0, y: body_top, w, h: (body_bottom - body_top).max(0.0) } }
    }

    /// The chapter rows (or thumbnail cards) on screen, with their chapter
    /// index. More chapters than fit: the page follows the playhead.
    fn cut_chapter_rows(&self) -> Vec<(RectPx, usize)> {
        let Some(cut) = &self.cut else { return Vec::new() };
        let side = self.cut_side();
        if self.workspace().side.w <= 0.0 || self.cut_tab != 0 || cut.chapters.is_empty() { return Vec::new(); }
        let body = side.body;
        let n = cut.chapters.len();
        let current = cut.chapters.iter().rposition(|c| c.start <= self.t + 1e-6).unwrap_or(0);
        if self.cut_thumbs {
            let (cw, ch) = ((body.w - 8.0) / 2.0, 104.0);
            let rows = (((body.h + 8.0) / (ch + 8.0)).floor().max(0.0)) as usize;
            let fit = rows * 2;
            let first = (current.saturating_sub(fit / 2).min(n.saturating_sub(fit)) / 2) * 2;
            (first..n.min(first + fit)).enumerate()
                .map(|(i, idx)| (RectPx { x: body.x + (i % 2) as f32 * (cw + 8.0), y: body.y + (i / 2) as f32 * (ch + 8.0), w: cw, h: ch }, idx))
                .collect()
        } else {
            let fit = (((body.h + 6.0) / 52.0).floor().max(0.0)) as usize;
            let first = current.saturating_sub(fit / 2).min(n.saturating_sub(fit));
            (first..n.min(first + fit)).enumerate()
                .map(|(i, idx)| (RectPx { x: body.x, y: body.y + i as f32 * 52.0, w: body.w, h: 46.0 }, idx))
                .collect()
        }
    }

    /// What the frame under the playhead is, on the footage: keyframe or
    /// not, its number, and the keyframes either side.
    fn build_cut_chips(&self, items: &mut Vec<Item>) {
        let Some(cut) = &self.cut else { return };
        let canvas = self.workspace().canvas;
        let image = self.content_rect(self.active);
        let (x, top) = (image.x.max(canvas.x) + 12.0, image.y.max(canvas.y) + 12.0);
        let bottom = (image.y + image.h).min(canvas.y + canvas.h) - 12.0;
        if bottom - top < 70.0 || cut.keys.is_empty() { return; }
        let frame = 1.0 / self.fps;
        let dark = [0.031, 0.035, 0.039, 0.93];
        let ink = mix(0x08090a, 0xffffff, 0.7);
        let mut chip = |x: f32, y: f32, text: String, bg: [f32; 4], color: [f32; 4]| -> f32 {
            let width = text.chars().count() as f32 * 10.5 * MONO_ADV + 16.0;
            items.push(Item::Text(TextItem { valign: VAlign::Middle,
                bg: Some(TextBg { radius: 5.0, pad_x: 8.0, pad_y: 5.0, ..TextBg::new(bg) }),
                ..TextItem::new(x + 8.0, y, 10.5, color, text) }));
            x + width + 6.0
        };
        let on_key = cut.on_key(self.t, frame);
        let next = if on_key { chip(x, top + 10.0, "I · KEY".into(), hex_color(CUT_AMBER), hex_color(CUT_AMBER_INK)) }
            else { chip(x, top + 10.0, "between keys".into(), dark, ink) };
        chip(next, top + 10.0, format!("frame {}", (self.t * self.fps).round() as i64), dark, ink);
        let frames = |a: f64, b: f64| ((b - a) * self.fps).round() as i64;
        let before = if on_key { cut.key_step(self.t, -1) } else { cut.prev_key(self.t) };
        let mut at = x;
        if before < self.t - frame * 0.5 {
            at = chip(at, bottom - 10.0, format!("◀ key {}  −{}f", fmt_tc(before, self.fps), frames(before, self.t)), dark, ink);
        }
        let after = cut.key_step(self.t, 1);
        if after > self.t + frame * 0.5 {
            chip(at, bottom - 10.0, format!("key {}  +{}f ▶", fmt_tc(after, self.fps), frames(self.t, after)), dark, ink);
        }
    }

    fn build_cut_hud(&self, items: &mut Vec<Item>, vp: (f32, f32)) {
        let l = self.workspace();
        let cut = self.cut.as_ref().unwrap();
        let rule = mix(CUT_BG, 0xffffff, 0.06);
        // Opaque shell surfaces, as in Input.
        items.push(Item::Rect(RectItem::new(RectPx { x: 0.0, y: 0.0, w: vp.0, h: HEADER_H }, WORKSPACE_PANEL)));
        items.push(Item::Rect(RectItem::new(l.transport, hex_color(CUT_BG))));
        items.push(Item::Rect(RectItem::new(l.timeline, hex_color(CUT_TL))));
        items.push(Item::Rect(RectItem::new(l.side, hex_color(CUT_BG))));
        for r in [RectPx { x: 0.0, y: HEADER_H - 1.0, w: vp.0, h: 1.0 },
            RectPx { h: 1.0, ..l.transport }, RectPx { h: 1.0, ..l.timeline },
            RectPx { w: if l.side.w > 0.0 { 1.0 } else { 0.0 }, ..l.side }] {
            items.push(Item::Rect(RectItem::new(r, rule)));
        }

        // ---- header: tabs, the clip, what an export will do ----
        let controls = self.controls();
        let tabs_end = controls.iter().filter(|c| matches!(c.action, Action::Tool(_))).map(|c| c.r.x + c.r.w).fold(0.0, f32::max);
        let header_right = controls.iter().filter(|c| matches!(c.action, Action::Cut(CutAction::Undo))).map(|c| c.r.x).next().unwrap_or(vp.0);
        let (promise, promise_color) = if cut.snap == Snap::Keyframe { ("LOSSLESS · STREAM COPY", hex_color(CUT_GREEN)) }
            else { ("FRAME-ACCURATE · RE-ENCODE", hex_color(CUT_CORAL_INK)) };
        let promise_w = if vp.0 >= 1000.0 { promise.chars().count() as f32 * 10.5 * MONO_ADV + 26.0 } else { 0.0 };
        if promise_w > 0.0 {
            items.push(Item::Text(TextItem { align: Align::Right, valign: VAlign::Middle, tracking: 0.6,
                ..TextItem::new(header_right - 14.0, HEADER_H / 2.0, 10.5, promise_color, promise) }));
        }
        ui_label(items, tabs_end + 16.0, HEADER_H / 2.0, 12.0, mix(0x050506, 0xffffff, 0.5), name_of(&cut.path),
            Align::Left, header_right - promise_w - tabs_end - 44.0);

        // ---- scrub toolbar: play, the clock, the edit tools, the zoom ----
        let bar = l.transport;
        let cy = bar.y + bar.h / 2.0;
        let tc = fmt_tc(self.t, self.fps);
        let (clock, frames) = tc.split_at(8);
        let x = self.cut_clock_x();
        items.push(Item::Text(TextItem { valign: VAlign::Middle, ..TextItem::new(x, cy, 20.0, hex_color(CUT_INK), clock) }));
        items.push(Item::Text(TextItem { valign: VAlign::Middle,
            ..TextItem::new(x + 8.0 * 20.0 * MONO_ADV, cy, 20.0, mix(CUT_BG, 0xffffff, 0.3), frames) }));
        let sep = mix(CUT_BG, 0xffffff, 0.1);
        items.push(Item::Rect(RectItem::new(RectPx { x: self.cut_tools_x() - 15.0, y: cy - 10.0, w: 1.0, h: 20.0 }, sep)));
        let zoom = self.cut_zoom_bar();
        items.push(Item::Rect(RectItem { radius: 1.5, ..RectItem::new(zoom, mix(CUT_BG, 0xffffff, 0.14)) }));
        let knob = zoom.x + zoom.w * cut.zoom_fraction(self.cut_lanes().video.w) as f32;
        items.push(Item::Rect(RectItem { radius: 5.5, ..RectItem::new(RectPx { x: knob - 5.5, y: cy - 5.5, w: 11.0, h: 11.0 }, hex_color(CUT_INK)) }));
        items.push(Item::Rect(RectItem::new(RectPx { x: zoom.x - 45.0, y: cy - 10.0, w: 1.0, h: 20.0 }, sep)));
        // The snap lamp: lit while every cut lands on a keyframe.
        let lamp = RectPx { x: zoom.x - 66.0, y: cy - 3.5, w: 7.0, h: 7.0 };
        if cut.snap == Snap::Keyframe {
            items.push(Item::Rect(RectItem { radius: 7.5, ..RectItem::new(RectPx { x: lamp.x - 4.0, y: lamp.y - 4.0, w: 15.0, h: 15.0 }, [0.208, 0.757, 0.373, 0.10]) }));
        }
        items.push(Item::Rect(RectItem { radius: 3.5, ..RectItem::new(lamp, if cut.snap == Snap::Keyframe { hex_color(CUT_GREEN) } else { mix(CUT_BG, 0xffffff, 0.2) }) }));

        self.build_cut_inspector(items);
        self.build_cut_timeline(items);
        for control in &controls {
            self.draw_cut_control(items, control);
        }
        // Icon buttons carry no words, so the hovered one names itself.
        if let Some(c) = controls.iter().find(|c| matches!(c.action, Action::Cut(a) if !matches!(a, CutAction::Undo | CutAction::Export))
            && contains(c.r, self.cursor.0, self.cursor.1) && self.cursor_inside) {
            let text = if c.hint.is_empty() { c.label.clone() } else { format!("{}  ·  {}", c.label, c.hint) };
            let right = c.r.x + c.r.w / 2.0 > vp.0 - 140.0;
            items.push(Item::Text(TextItem { align: if right { Align::Right } else { Align::Left }, valign: VAlign::Middle,
                bg: Some(TextBg { radius: 5.0, pad_x: 8.0, pad_y: 5.0, ..TextBg::new(hex_color(0x26282b)) }),
                ..TextItem::new(if right { c.r.x + c.r.w - 8.0 } else { c.r.x + 8.0 }, c.r.y - 16.0, 10.5, hex_color(CUT_INK), text) }));
        }
    }

    /// One header tab, header button or toolbar icon.
    fn draw_cut_control(&self, items: &mut Vec<Item>, control: &Control) {
        let cut = self.cut.as_ref().unwrap();
        let hovered = contains(control.r, self.cursor.0, self.cursor.1) && self.cursor_inside;
        let r = control.r;
        let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
        let Action::Cut(action) = control.action else {
            if control.selected || hovered {
                items.push(Item::Rect(RectItem { radius: 7.0, ..RectItem::new(r, if control.selected { TOOL_BG } else { CONTROL_BG }) }));
            }
            let color = if control.selected { ACCENT } else if hovered { WORKSPACE_TEXT } else { WORKSPACE_DIM };
            ui_label(items, cx, cy, 11.0, color, &control.label, Align::Center, r.w - 8.0);
            return;
        };
        let soft = mix(CUT_BG, 0xffffff, 0.72);
        match action {
            CutAction::Export => {
                let busy = cut.exporting() || cut.cuts().is_empty();
                let fill = if busy { mix(0x050506, CUT_INK, 0.35) } else if hovered { hex_color(0xffffff) } else { hex_color(CUT_INK) };
                items.push(Item::Rect(RectItem { radius: 7.0, ..RectItem::new(r, fill) }));
                ui_label(items, cx, cy, 11.5, hex_color(CUT_BG), &control.label, Align::Center, r.w - 8.0);
            }
            CutAction::Undo => {
                let dead = !cut.can_undo();
                items.push(Item::Rect(RectItem { radius: 7.0, border_w: 1.0,
                    border_color: mix(CUT_BG, 0xffffff, if hovered && !dead { 0.3 } else { 0.14 }),
                    ..RectItem::new(r, if hovered && !dead { mix(CUT_BG, 0xffffff, 0.05) } else { [0.0; 4] }) }));
                let color = if dead { mix(CUT_BG, 0xffffff, 0.3) } else { soft };
                let cap = control.hint.chars().count() as f32 * 10.0 * MONO_ADV + 12.0;
                ui_label(items, r.x + 12.0, cy, 11.5, color, &control.label, Align::Left, r.w - cap - 26.0);
                let key = RectPx { x: r.x + r.w - 6.0 - cap.max(20.0), y: cy - 10.0, w: cap.max(20.0), h: 20.0 };
                items.push(Item::Rect(RectItem { radius: 5.0, ..RectItem::new(key, mix(CUT_BG, 0xffffff, 0.09)) }));
                ui_label(items, key.x + key.w / 2.0, cy, 10.0, color, control.hint, Align::Center, key.w);
            }
            CutAction::Play => {
                items.push(Item::Rect(RectItem { radius: 15.0, ..RectItem::new(r, ACCENT) }));
                let white = hex_color(0xffffff);
                if self.playing {
                    for x in [cx - 5.0, cx + 2.0] {
                        items.push(Item::Rect(RectItem::new(RectPx { x, y: cy - 6.0, w: 3.0, h: 12.0 }, white)));
                    }
                } else {
                    items.push(Item::Triangle { r: RectPx { x: cx - 3.5, y: cy - 6.0, w: 10.0, h: 12.0 }, color: white, left: false, radius: 1.0 });
                }
            }
            CutAction::Zoom(zoom_in) => {
                let color = if hovered { hex_color(CUT_INK) } else { mix(CUT_BG, 0xffffff, 0.5) };
                items.push(Item::Rect(RectItem::new(RectPx { x: cx - 5.0, y: cy - 0.75, w: 10.0, h: 1.5 }, color)));
                if zoom_in {
                    items.push(Item::Rect(RectItem::new(RectPx { x: cx - 0.75, y: cy - 5.0, w: 1.5, h: 10.0 }, color)));
                }
            }
            _ => {
                let dead = action == CutAction::Apply && cut.selection().is_none();
                let fill = if control.selected { Some(TOOL_BG) } else if hovered && !dead { Some(mix(CUT_BG, 0xffffff, 0.07)) } else { None };
                if let Some(fill) = fill {
                    items.push(Item::Rect(RectItem { radius: 8.0, ..RectItem::new(r, fill) }));
                }
                let (icon, color) = match action {
                    CutAction::Step(d) => (if d < 0 { Icon::StepBack } else { Icon::StepFwd }, soft),
                    CutAction::Keyframe(d) => (if d < 0 { Icon::KeyBack } else { Icon::KeyFwd }, soft),
                    CutAction::Chapter(d) => (if d < 0 { Icon::ChapBack } else { Icon::ChapFwd }, soft),
                    CutAction::In => (Icon::In, hex_color(CUT_IN)),
                    CutAction::Out => (Icon::Out, hex_color(CUT_OUT)),
                    CutAction::Split => (Icon::Split, soft),
                    CutAction::Apply => (Icon::Trash, hex_color(CUT_HOT)),
                    _ => (Icon::Magnet, if control.selected { ACCENT } else { soft }),
                };
                draw_icon(items, icon, cx, cy, if dead { mix(CUT_BG, 0xffffff, 0.25) } else { color });
            }
        }
    }

    fn build_cut_inspector(&self, items: &mut Vec<Item>) {
        let s = self.workspace().side;
        if s.w <= 0.0 { return; }
        let cut = self.cut.as_ref().unwrap();
        let g = self.cut_side();
        let (x0, x1) = (g.body.x, g.body.x + g.body.w);
        let dim = mix(CUT_BG, 0xffffff, 0.5);
        let faint = mix(CUT_BG, 0xffffff, 0.42);
        let ink = hex_color(CUT_INK);
        let green = hex_color(CUT_GREEN_INK);
        let chip_bg = mix(CUT_BG, 0xffffff, 0.07);
        items.push(Item::Clip(Some(s)));

        // ---- the icon tabs: chapters, streams ----
        items.push(Item::Rect(RectItem { radius: 9.0, ..RectItem::new(g.tabs, mix(CUT_BG, 0xffffff, 0.05)) }));
        for (i, (r, icon)) in g.tab.iter().zip([Icon::Flag, Icon::Film]).enumerate() {
            let on = self.cut_tab == i;
            let hovered = self.cursor_inside && contains(*r, self.cursor.0, self.cursor.1);
            if on { items.push(Item::Rect(RectItem { radius: 7.0, ..RectItem::new(*r, hex_color(CUT_TOOL_ON)) })); }
            draw_icon(items, icon, r.x + r.w / 2.0, r.y + r.h / 2.0, if on { ACCENT } else if hovered { ink } else { dim });
        }

        // A chip: text on a rounded ground, left edge at `x`. Returns the next x.
        let chip = |items: &mut Vec<Item>, x: f32, y: f32, text: &str, bg: [f32; 4], fg: [f32; 4]| -> f32 {
            items.push(Item::Text(TextItem { valign: VAlign::Middle,
                bg: Some(TextBg { radius: 5.0, pad_x: 7.0, pad_y: 5.0, ..TextBg::new(bg) }),
                ..TextItem::new(x + 7.0, y, 10.0, fg, text) }));
            x + text.chars().count() as f32 * 10.0 * MONO_ADV + 14.0 + 6.0
        };
        let kv = |items: &mut Vec<Item>, y: f32, key: &str, value: String, color: [f32; 4]| {
            ui_label(items, x0, y, 11.5, dim, key, Align::Left, 110.0);
            ui_label(items, x1, y, 11.5, color, value, Align::Right, 170.0);
        };

        if self.cut_tab == 0 {
            // ---- chapters ----
            let facts_chips: Vec<String> = if cut.facts.chips.is_empty() {
                let v = &self.videos[self.active].info;
                vec![v.codec.to_uppercase(), format!("{}p", v.height), format!("{:.3}", v.fps).trim_end_matches('0').trim_end_matches('.').to_string()]
            } else { cut.facts.chips.clone() };
            let mut x = x0;
            let mut y = g.chips_y + 10.0;
            for (i, text) in facts_chips.iter().enumerate() {
                let w = text.chars().count() as f32 * 10.0 * MONO_ADV + 20.0;
                if x + w > x1 { x = x0; y += 26.0; }
                x = if i == 0 { chip(items, x, y, text, mix(CUT_BG, CUT_IN, 0.16), hex_color(0x6ab8f7)) }
                    else { chip(items, x, y, text, chip_bg, mix(CUT_BG, 0xffffff, 0.75)) };
            }
            let cy = g.count_y + 12.0;
            ui_label(items, x0, cy, 11.0, dim, cut.chapters.len().to_string(), Align::Left, 40.0);
            let plus_hot = self.cursor_inside && contains(g.add_btn, self.cursor.0, self.cursor.1);
            items.push(Item::Rect(RectItem { radius: 7.0, ..RectItem::new(g.add_btn, mix(CUT_BG, 0xffffff, if plus_hot { 0.12 } else { 0.07 })) }));
            let (px, py) = (g.add_btn.x + g.add_btn.w / 2.0, g.add_btn.y + g.add_btn.h / 2.0);
            let plus = if plus_hot { ink } else { dim };
            items.push(Item::Rect(RectItem::new(RectPx { x: px - 5.0, y: py - 0.75, w: 10.0, h: 1.5 }, plus)));
            items.push(Item::Rect(RectItem::new(RectPx { x: px - 0.75, y: py - 5.0, w: 1.5, h: 10.0 }, plus)));
            items.push(Item::Rect(RectItem { radius: 7.0, ..RectItem::new(g.toggle, mix(CUT_BG, 0xffffff, 0.05)) }));
            for (r, icon, on) in [(g.list_btn, Icon::List, !self.cut_thumbs), (g.grid_btn, Icon::Grid, self.cut_thumbs)] {
                if on { items.push(Item::Rect(RectItem { radius: 5.0, ..RectItem::new(r, hex_color(CUT_TOOL_ON)) })); }
                draw_icon(items, icon, r.x + r.w / 2.0, r.y + r.h / 2.0, if on { ACCENT } else { dim });
            }
            let current = cut.chapters.iter().rposition(|c| c.start <= self.t + 1e-6);
            if cut.chapters.is_empty() {
                let msg = if cut.scanning() { "reading chapters …" } else { "no chapters in this file" };
                ui_label(items, x0 + g.body.w / 2.0, g.body.y + 20.0, 11.0, faint, msg, Align::Center, g.body.w);
            }
            for (r, idx) in self.cut_chapter_rows() {
                let c = &cut.chapters[idx];
                let end = cut.chapters.get(idx + 1).map_or(cut.duration, |n| n.start);
                let on = current == Some(idx);
                let hovered = self.cursor_inside && contains(r, self.cursor.0, self.cursor.1);
                let snap_dot = if cut.on_key(c.start, 1.0 / self.fps.max(1.0)) { hex_color(CUT_GREEN) } else { hex_color(CUT_CORAL) };
                let number = format!("{:02}", idx + 1);
                let (num_bg, num_fg) = if on { (hex_color(CUT_AMBER), hex_color(CUT_AMBER_INK)) } else { (mix(CUT_BG, 0xffffff, 0.08), mix(CUT_BG, 0xffffff, 0.6)) };
                let ground = if on { mix(CUT_BG, CUT_AMBER, 0.12) } else if hovered { mix(CUT_BG, 0xffffff, 0.07) } else { hex_color(CUT_LANE) };
                if self.cut_thumbs {
                    items.push(Item::Rect(RectItem { radius: 9.0, border_w: if on { 1.0 } else { 0.0 }, border_color: hex_color(CUT_AMBER), ..RectItem::new(r, ground) }));
                    // No frame to show yet: the film ground the picture clip uses.
                    let pic = RectPx { x: r.x + 5.0, y: r.y + 5.0, w: r.w - 10.0, h: 62.0 };
                    items.push(Item::Rect(RectItem { radius: 5.0, ..RectItem::new(pic, hex_color(CUT_FILM)) }));
                    items.push(Item::Rect(RectItem { radius: 6.0, ..RectItem::new(RectPx { x: pic.x + 4.0, y: pic.y + 4.0, w: 20.0, h: 18.0 }, if on { num_bg } else { [0.0, 0.0, 0.0, 0.6] }) }));
                    ui_label(items, pic.x + 14.0, pic.y + 13.0, 9.5, if on { num_fg } else { mix(CUT_BG, 0xffffff, 0.8) }, number, Align::Center, 20.0);
                    items.push(Item::Rect(RectItem { radius: 3.5, ..RectItem::new(RectPx { x: pic.x + pic.w - 12.0, y: pic.y + 5.0, w: 7.0, h: 7.0 }, snap_dot) }));
                    ui_label(items, r.x + 8.0, r.y + 80.0, 12.0, mix(CUT_BG, 0xffffff, 0.88), c.title.as_str(), Align::Left, r.w - 16.0);
                    ui_label(items, r.x + 8.0, r.y + 95.0, 10.0, faint, format!("{} · {}", fmt_hms(c.start), fmt_span(end - c.start)), Align::Left, r.w - 16.0);
                } else {
                    items.push(Item::Rect(RectItem { radius: 8.0, border_w: if on { 1.0 } else { 0.0 }, border_color: hex_color(CUT_AMBER), ..RectItem::new(r, ground) }));
                    let num = RectPx { x: r.x + 10.0, y: r.y + (r.h - 22.0) / 2.0, w: 22.0, h: 22.0 };
                    items.push(Item::Rect(RectItem { radius: 6.0, ..RectItem::new(num, num_bg) }));
                    ui_label(items, num.x + 11.0, num.y + 11.0, 10.5, num_fg, number, Align::Center, 22.0);
                    ui_label(items, r.x + 42.0, r.y + 16.0, 12.0, mix(CUT_BG, 0xffffff, 0.88), c.title.as_str(), Align::Left, r.w - 42.0 - 28.0);
                    ui_label(items, r.x + 42.0, r.y + 32.0, 10.0, faint, format!("{} · {}", fmt_hms(c.start), fmt_span(end - c.start)), Align::Left, r.w - 42.0 - 28.0);
                    items.push(Item::Rect(RectItem { radius: 3.5, ..RectItem::new(RectPx { x: r.x + r.w - 17.0, y: r.y + r.h / 2.0 - 3.5, w: 7.0, h: 7.0 }, snap_dot) }));
                }
            }
        } else {
            // ---- streams ----
            let section = |items: &mut Vec<Item>, y: &mut f32, icon: Icon, title: &str, rows: Vec<(String, String, [f32; 4])>| {
                draw_icon(items, icon, x0 + 8.0, *y + 8.0, dim);
                caps_label(items, x0 + 24.0, *y + 8.0, title);
                *y += 24.0;
                for (k, v, color) in rows {
                    kv(items, *y + 11.0, &k, v, color);
                    *y += 22.0;
                }
                *y += 14.0;
            };
            let mut y = g.body.y;
            let facts = &cut.facts;
            let rows = |r: &Vec<(&'static str, String)>| r.iter().map(|(k, v)| (k.to_string(), v.clone(), ink)).collect::<Vec<_>>();
            let v = &self.videos[self.active].info;
            let video_title = if facts.video_title.is_empty() { v.codec.to_uppercase() } else { facts.video_title.to_uppercase().replace("·", "·") };
            let video_rows = if facts.video.is_empty() {
                vec![("Size".into(), format!("{} × {}", v.width, v.height), ink), ("Rate".into(), format!("{:.3} fps", v.fps), ink)]
            } else { rows(&facts.video) };
            section(items, &mut y, Icon::Film, &video_title, video_rows);
            if !facts.audio_title.is_empty() {
                section(items, &mut y, Icon::Speaker, &facts.audio_title.to_uppercase(), rows(&facts.audio));
            }
            if !facts.subs_title.is_empty() || !cut.cues.is_empty() {
                let title = if facts.subs_title.is_empty() { "SUBTITLES".to_string() } else { facts.subs_title.to_uppercase() };
                section(items, &mut y, Icon::Caption, &title, vec![("Cues".into(), cut.cues.len().to_string(), ink)]);
            }
            if let Some((count, median, longest)) = cut.gop_stats() {
                let long = longest > median * 2.0 + 0.5;
                section(items, &mut y, Icon::Ticks, "KEYFRAMES", vec![
                    ("Count".into(), format!("{count}"), ink),
                    ("GOP".into(), format!("{median:.1} s · max {longest:.1} s"), if long { hex_color(CUT_CORAL_INK) } else { ink }),
                ]);
            } else if cut.scanning() {
                section(items, &mut y, Icon::Ticks, "KEYFRAMES", vec![("Count".into(), "reading …".into(), faint)]);
            }
        }

        // ---- the snapping warning, when there is one ----
        if !g.warning.is_empty() {
            let r = RectPx { x: x0, y: g.warning_y, w: x1 - x0, h: g.warning_h };
            items.push(Item::Rect(RectItem { radius: 8.0, border_w: 1.0, border_color: mix(CUT_BG, CUT_CORAL, 0.28),
                ..RectItem::new(r, mix(CUT_BG, CUT_CORAL, 0.10)) }));
            for (i, line) in g.warning.iter().enumerate() {
                ui_label(items, r.x + 12.0, r.y + 15.0 + i as f32 * 15.0, 10.0, hex_color(CUT_CORAL_INK), line.as_str(), Align::Left, r.w - 24.0);
            }
        }

        // ---- footer: what the export will be ----
        items.push(Item::Rect(RectItem::new(RectPx { x: x0, y: g.footer_y, w: x1 - x0, h: 1.0 }, mix(CUT_BG, 0xffffff, 0.07))));
        let cy = g.footer_y + 28.0;
        let kept = cut.kept();
        let removed = cut.duration - kept;
        let frame_mode = cut.snap == Snap::Frame;
        let right_chip = |items: &mut Vec<Item>, right: f32, text: &str, bg: [f32; 4], fg: [f32; 4]| -> f32 {
            let w = text.chars().count() as f32 * 10.0 * MONO_ADV + 14.0;
            items.push(Item::Text(TextItem { align: Align::Right, valign: VAlign::Middle,
                bg: Some(TextBg { radius: 5.0, pad_x: 7.0, pad_y: 5.0, ..TextBg::new(bg) }),
                ..TextItem::new(right - 7.0, cy, 10.0, fg, text) }));
            right - w - 6.0
        };
        if self.cut_tab == 0 {
            ui_label(items, x0, cy, 15.0, ink, fmt_hms(kept), Align::Left, 110.0);
            let (re_text, re_bg, re_fg) = if frame_mode { ("all re-enc", mix(CUT_BG, CUT_CORAL, 0.14), hex_color(CUT_CORAL_INK)) }
                else { ("0 re-enc", mix(CUT_BG, CUT_GREEN, 0.14), green) };
            let r = right_chip(items, x1, re_text, re_bg, re_fg);
            if removed > 1e-3 {
                right_chip(items, r, &format!("−{}", fmt_span(removed)), chip_bg, mix(CUT_BG, 0xffffff, 0.75));
            }
        } else {
            ui_label(items, x0, cy, 15.0, ink, fmt_hms(cut.duration), Align::Left, 110.0);
            let ext = cut.path.extension().map(|e| e.to_string_lossy().to_uppercase()).unwrap_or_default();
            let r = right_chip(items, x1, &ext, chip_bg, mix(CUT_BG, 0xffffff, 0.75));
            right_chip(items, r, &fmt_size(self.videos[self.active].info.file_size), chip_bg, mix(CUT_BG, 0xffffff, 0.75));
        }
        items.push(Item::Clip(None));
    }

    /// The scrub panel under the toolbar — the design's variation A,
    /// "clips on tracks": every kept piece is a clip with a grip at each
    /// end, every removed one a dashed ghost, on one time axis with the
    /// ruler, chapter flags, subtitle cues and keyframe ticks.
    fn build_cut_timeline(&self, items: &mut Vec<Item>) {
        let tl = self.workspace().timeline;
        let lanes = self.cut_lanes();
        let cut = self.cut.as_ref().unwrap();
        let v = lanes.video;
        let (t0, pps) = (cut.t0, cut.pps.max(1e-6));
        let (a, b) = (t0, t0 + cut.span(v.w));
        let x_of = |t: f64| v.x + ((t - t0) * pps) as f32;
        let faint = mix(CUT_TL, 0xffffff, 0.25);
        let line = |items: &mut Vec<Item>, x: f32, y: f32, h: f32, w: f32, color: [f32; 4]| {
            items.push(Item::Rect(RectItem::new(RectPx { x, y, w, h }, color)));
        };
        let column = RectPx { x: v.x, y: tl.y + 1.0, w: v.w, h: tl.h - 1.0 };

        let gutter = mix(CUT_TL, 0xffffff, 0.42);
        for (lane, icon) in [(lanes.chapters, Icon::Flag), (Some(v), Icon::Film), (lanes.audio, Icon::Speaker),
            (Some(lanes.subs), Icon::Caption), (Some(lanes.keys), Icon::Ticks)] {
            if let Some(r) = lane { draw_icon(items, icon, tl.x + 30.0, r.y + r.h / 2.0, gutter); }
        }

        // ---- ruler: a label at every round step that leaves it room ----
        items.push(Item::Clip(Some(column)));
        let step = [1.0, 2.0, 5.0, 10.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0]
            .into_iter().find(|s| s * pps >= 96.0).unwrap_or(7200.0);
        let mut t = (a / step).ceil() * step;
        while t < b {
            let x = x_of(t);
            line(items, x, lanes.ruler.y, 14.0, 1.0, mix(CUT_TL, 0xffffff, 0.2));
            let s = t.round() as u64;
            let label = if cut.duration >= 3600.0 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{:02}:{:02}", s / 60, s % 60) };
            ui_label(items, x + 5.0, lanes.ruler.y + 7.0, 9.5, mix(CUT_TL, 0xffffff, 0.4), label, Align::Left, 80.0);
            t += step;
        }

        // ---- chapters: a numbered flag each, the current one lit ----
        if let Some(r) = lanes.chapters {
            let current = cut.chapters.iter().rposition(|c| c.start <= self.t + 1e-6);
            for (i, c) in cut.chapters.iter().enumerate().filter(|(_, c)| c.start >= a - 40.0 / pps && c.start <= b) {
                let x = x_of(c.start);
                let lit = current == Some(i);
                let (bg, fg, stem) = if lit { (hex_color(CUT_AMBER), hex_color(CUT_AMBER_INK), hex_color(CUT_AMBER)) }
                    else { (mix(CUT_TL, 0xffffff, 0.16), hex_color(CUT_INK), mix(CUT_TL, 0xffffff, 0.35)) };
                items.push(Item::Text(TextItem { valign: VAlign::Middle,
                    bg: Some(TextBg { radius: 5.0, pad_x: 6.0, pad_y: 3.0, ..TextBg::new(bg) }),
                    ..TextItem::new(x + 6.0, r.y + 9.0, 10.5, fg, (i + 1).to_string()) }));
                line(items, x, r.y + 18.0, 6.0, 1.0, stem);
            }
        }

        // ---- video and audio: one clip per piece ----
        let segments = cut.segments();
        let dashes = |items: &mut Vec<Item>, r: RectPx| {
            let color = mix(CUT_TL, 0xffffff, 0.3);
            let mut x = r.x;
            while x < r.x + r.w {
                let w = 4.0_f32.min(r.x + r.w - x);
                line(items, x, r.y, 1.0, w, color);
                line(items, x, r.y + r.h - 1.0, 1.0, w, color);
                x += 8.0;
            }
            let mut y = r.y;
            while y < r.y + r.h {
                let h = 4.0_f32.min(r.y + r.h - y);
                line(items, r.x, y, h, 1.0, color);
                line(items, r.x + r.w - 1.0, y, h, 1.0, color);
                y += 8.0;
            }
        };
        let grips = |items: &mut Vec<Item>, r: RectPx| {
            if r.w < 28.0 { return; }
            for x in [r.x + 3.0, r.x + 6.0, r.x + r.w - 7.0, r.x + r.w - 4.0] {
                // Only the ends that are on screen can be taken hold of.
                if x > v.x && x < v.x + v.w { line(items, x, r.y + 9.0, r.h - 18.0, 1.0, [1.0, 1.0, 1.0, 0.22]); }
            }
        };
        for (lane, audio) in [(Some(v), false), (lanes.audio, true)] {
            let Some(row) = lane else { continue };
            for (i, g) in segments.iter().enumerate().filter(|(_, g)| g.end > a && g.start < b) {
                let r = RectPx { x: x_of(g.start) + 1.0, y: row.y, w: (x_of(g.end) - x_of(g.start) - 2.0).max(1.0), h: row.h };
                if g.cut {
                    dashes(items, r);
                } else if audio {
                    items.push(Item::Rect(RectItem { radius: 7.0, border_w: 1.0, border_color: mix(CUT_TL, 0x1580de, 0.6),
                        ..RectItem::new(r, hex_color(if i % 2 == 1 { CUT_SOUND_ALT } else { CUT_SOUND })) }));
                } else {
                    // The film ground: the board's 135° stripes (#2a2d30 / #202326, 14px).
                    items.push(Item::Hatch { r, a: hex_color(0x2a2d30), b: hex_color(0x202326), radius: 7.0, period: 14.0 });
                    items.push(Item::Rect(RectItem { radius: 7.0, border_w: 1.0, border_color: mix(CUT_TL, 0xffffff, 0.2),
                        ..RectItem::new(r, [0.0; 4]) }));
                    // Keyframes: a hairline through the picture, a short bright
                    // foot at the bottom edge, and the two the selection snapped
                    // to drawn tall. Thinned when closer than 6px — never smeared.
                    let snapped = [cut.in_snapped(), cut.out_snapped()];
                    let mut last = f32::MIN;
                    for key in cut.keys_in(g.start, g.end) {
                        let x = x_of(*key);
                        if x < r.x + 3.0 || x > r.x + r.w - 3.0 || x < v.x || x > v.x + v.w { continue; }
                        let tall = cut.snap == Snap::Keyframe && snapped.iter().flatten().any(|t| (t - key).abs() < 1e-3);
                        if x - last < 6.0 && !tall { continue; }
                        last = x;
                        if tall {
                            line(items, x - 0.5, r.y + 1.0, r.h - 2.0, 1.5, mix(CUT_FILM, CUT_HEAD, 0.85));
                        } else {
                            line(items, x, r.y + 1.0, r.h - 2.0, 1.0, mix(CUT_FILM, 0xffffff, 0.12));
                            line(items, x, r.y + r.h - 7.0, 6.0, 1.0, mix(CUT_FILM, 0xffffff, 0.5));
                        }
                    }
                }
                if audio && !cut.wave.is_empty() {
                    let (from, to) = ((r.x + 12.0).max(v.x), (r.x + r.w - 12.0).min(v.x + v.w));
                    let mut x = from;
                    while x < to {
                        let ta = t0 + ((x - v.x) as f64) / pps;
                        let Some(e) = cut.peak(ta, ta + 3.0 / pps) else { break };
                        let h = (e.sqrt() * 40.0).max(3.0).round();
                        items.push(Item::Rect(RectItem { radius: 1.0, ..RectItem::new(
                            RectPx { x, y: r.y + ((r.h - h) / 2.0).round(), w: 2.0, h },
                            mix(if g.cut { CUT_TL } else { CUT_SOUND }, CUT_WAVE, if g.cut { 0.22 } else { 0.95 })) }));
                        x += 3.0;
                    }
                }
                if !g.cut { grips(items, r); }
            }
            if audio && cut.wave.is_empty() {
                ui_label(items, row.x + 16.0, row.y + row.h / 2.0, 10.0, mix(CUT_SOUND, 0xffffff, 0.4),
                    if cut.reading_wave() { "reading audio…" } else { "no audio in this file" }, Align::Left, row.w);
            }
        }

        // ---- subtitles: a bar per cue ----
        let s = lanes.subs;
        for c in cut.cues.iter().filter(|c| c.end > a && c.start < b) {
            items.push(Item::Rect(RectItem { radius: 2.0,
                ..RectItem::new(RectPx { x: x_of(c.start), y: s.y + 4.0, w: (((c.end - c.start) * pps) as f32).max(3.0), h: 4.0 }, hex_color(CUT_CUE)) }));
        }

        // ---- keyframes: a tick each; the two the selection snapped to stand tall ----
        let k = lanes.keys;
        let snapped = [cut.in_snapped(), cut.out_snapped()];
        let mut last = f32::MIN;
        for key in cut.keys_in(a, b) {
            let x = x_of(*key);
            let tall = cut.snap == Snap::Keyframe && snapped.iter().flatten().any(|t| (t - key).abs() < 1e-3);
            // Denser than the lane can show: thin them, never smear.
            if x - last < 2.0 && !tall { continue; }
            last = x;
            if tall { line(items, x - 0.5, k.y, k.h, 2.0, hex_color(CUT_HEAD)); }
            else { line(items, x, k.y + k.h - 10.0, 10.0, 1.0, mix(CUT_TL, 0xffffff, 0.34)); }
        }
        if cut.keys.is_empty() {
            ui_label(items, k.x + 2.0, k.y + k.h / 2.0, 10.0, faint,
                if cut.scanning() { "scanning keyframes…" } else { "no keyframes found" }, Align::Left, k.w);
        }

        // ---- over the rows: in / out, the razor, then the playhead ----
        let (top, bottom) = (v.y - 6.0, s.y);
        let dashed = |items: &mut Vec<Item>, x: f32, y0: f32, y1: f32, w: f32, color: [f32; 4]| {
            let mut y = y0;
            while y < y1 {
                line(items, x, y, 4.0_f32.min(y1 - y), w, color);
                y += 8.0;
            }
        };
        for (asked, at, color, left) in [(cut.in_req, cut.in_snapped(), CUT_IN, true), (cut.out_req, cut.out_snapped(), CUT_OUT, false)] {
            let (Some(asked), Some(at)) = (asked, at) else { continue };
            if (asked - at).abs() > 1e-3 { dashed(items, x_of(asked), top, bottom, 1.0, mix(CUT_TL, 0xffffff, 0.45)); }
            let x = x_of(at);
            items.push(Item::Rect(RectItem { radius: 1.5, ..RectItem::new(RectPx { x: x - 1.5, y: top, w: 3.0, h: bottom - top }, hex_color(color)) }));
            // The tab hangs off the side the selection is NOT on.
            items.push(Item::Rect(RectItem { radius: 3.0,
                ..RectItem::new(RectPx { x: if left { x - 9.5 } else { x - 1.5 }, y: top, w: 11.0, h: 16.0 }, hex_color(color)) }));
        }
        if let Some(at) = self.cut_razor() {
            let x = x_of(at);
            let razor_bottom = lanes.audio.map_or(v.y + v.h, |r| r.y + r.h);
            dashed(items, x - 0.75, v.y, razor_bottom, 1.5, mix(CUT_FILM, 0xffffff, 0.8));
            items.push(Item::Rect(RectItem { radius: 12.0, ..RectItem::new(RectPx { x: x - 12.0, y: v.y - 14.0, w: 24.0, h: 24.0 }, hex_color(CUT_INK)) }));
            draw_icon(items, Icon::Split, x, v.y - 2.0, hex_color(CUT_BG));
        }
        // The flags carry numbers and the cues no words, so the one under
        // the pointer says what it is.
        let (mx, my) = self.cursor;
        let over = |r: RectPx| self.cursor_inside && self.cut_drag.is_none() && mx >= v.x && mx < v.x + v.w && my >= r.y && my < r.y + r.h;
        let at = self.cut_time_at(mx);
        let told = if lanes.chapters.is_some_and(over) {
            cut.chapters.iter().enumerate().find(|(_, c)| { let x = x_of(c.start); mx >= x - 2.0 && mx < x + 26.0 })
                .map(|(i, c)| format!("{} · {}", i + 1, c.title))
        } else if over(RectPx { y: s.y - 2.0, h: s.h + 4.0, ..s }) {
            cut.cues.iter().find(|c| at >= c.start && at < c.end).map(|c| c.text.clone())
        } else { None };
        if let Some(text) = told {
            let right = mx > v.x + v.w * 0.6;
            items.push(Item::Text(TextItem { align: if right { Align::Right } else { Align::Left }, valign: VAlign::Middle,
                bg: Some(TextBg { radius: 5.0, pad_x: 8.0, pad_y: 5.0, ..TextBg::new(hex_color(0x26282b)) }),
                max_width: Some(v.w * 0.5),
                ..TextItem::new(if right { mx - 10.0 } else { mx + 10.0 }, my - 20.0, 10.5, hex_color(CUT_INK), text) }));
        }
        let px = x_of(self.t);
        let head = hex_color(CUT_HEAD);
        // One white pixel, with a pixel of translucent black either side so it
        // holds against pale and dark footage alike.
        line(items, px - 1.5, tl.y + 1.0, tl.h - 1.0, 1.0, [0.0, 0.0, 0.0, 0.4]);
        line(items, px + 0.5, tl.y + 1.0, tl.h - 1.0, 1.0, [0.0, 0.0, 0.0, 0.4]);
        line(items, px - 0.5, tl.y + 1.0, tl.h - 1.0, 1.0, head);
        // The pin: a short tab with a point, three grooves to take hold of.
        items.push(Item::Rect(RectItem { radius: 2.0, ..RectItem::new(RectPx { x: px - 9.0, y: tl.y, w: 18.0, h: 9.0 }, head) }));
        items.push(Item::TriangleDown { r: RectPx { x: px - 9.0, y: tl.y + 7.0, w: 18.0, h: 7.0 }, color: head, radius: 0.5 });
        for x in [px - 3.5, px - 0.5, px + 2.5] {
            line(items, x, tl.y + 2.0, if x == px - 0.5 { 6.0 } else { 4.5 }, 1.2, mix(CUT_HEAD, CUT_TL, 0.55));
        }
        let tc = fmt_tc(self.t, self.fps);
        let chip = if cut.duration >= 3600.0 { tc.as_str() } else { &tc[3..] };
        let right = px + 110.0 > v.x + v.w;
        items.push(Item::Text(TextItem { align: if right { Align::Right } else { Align::Left }, valign: VAlign::Middle,
            bg: Some(TextBg { radius: 5.0, pad_x: 7.0, pad_y: 3.0, ..TextBg::new([0.02, 0.02, 0.024, 0.97]) }),
            ..TextItem::new(if right { px - 21.0 } else { px + 21.0 }, tl.y + 9.0, 10.5, hex_color(CUT_INK), chip) }));
        items.push(Item::Clip(None));
    }
}

/// The scrub panel's pictograms. The renderer draws rects and triangles,
/// not paths, so each is built from those on a 16px box about its centre.
#[derive(Clone, Copy)]
enum Icon { In, Out, Split, Trash, Magnet, Flag, Film, Speaker, Caption, Ticks, List, Grid, StepBack, StepFwd, KeyBack, KeyFwd, ChapBack, ChapFwd }

fn draw_icon(items: &mut Vec<Item>, icon: Icon, cx: f32, cy: f32, color: [f32; 4]) {
    let bar = |items: &mut Vec<Item>, x: f32, y: f32, w: f32, h: f32| {
        items.push(Item::Rect(RectItem { radius: 0.75, ..RectItem::new(RectPx { x: cx + x, y: cy + y, w, h }, color) }));
    };
    match icon {
        // An arrow running into a bracket: the range starts / ends here.
        Icon::In | Icon::Out => {
            let m = if matches!(icon, Icon::In) { 1.0 } else { -1.0 };
            let at = |x: f32, w: f32| if m > 0.0 { x } else { -x - w };
            bar(items, at(0.0, 1.5), -8.0, 1.5, 16.0);
            bar(items, at(0.0, 6.0), -8.0, 6.0, 1.5);
            bar(items, at(0.0, 6.0), 6.5, 6.0, 1.5);
            bar(items, at(-8.0, 6.0), -0.75, 6.0, 1.5);
            items.push(Item::Triangle { r: RectPx { x: cx + at(-3.5, 5.5), y: cy - 4.0, w: 5.5, h: 8.0 }, color, left: m < 0.0, radius: 0.5 });
        }
        // A blade between two pieces pulled apart.
        Icon::Split => {
            for y in [-8.0, -2.0, 4.0] { bar(items, -0.75, y, 1.5, 4.0); }
            items.push(Item::Triangle { r: RectPx { x: cx - 8.0, y: cy - 4.0, w: 5.5, h: 8.0 }, color, left: true, radius: 0.5 });
            items.push(Item::Triangle { r: RectPx { x: cx + 2.5, y: cy - 4.0, w: 5.5, h: 8.0 }, color, left: false, radius: 0.5 });
        }
        Icon::Trash => {
            bar(items, -7.0, -5.5, 14.0, 1.5);
            bar(items, -2.5, -8.0, 5.0, 1.5);
            bar(items, -2.5, -8.0, 1.5, 3.0);
            bar(items, 1.0, -8.0, 1.5, 3.0);
            bar(items, -5.0, -4.0, 1.5, 12.0);
            bar(items, 3.5, -4.0, 1.5, 12.0);
            bar(items, -5.0, 6.5, 10.0, 1.5);
            bar(items, -1.9, -1.5, 1.5, 6.0);
            bar(items, 0.6, -1.5, 1.5, 6.0);
        }
        // A horseshoe: two poles and the yoke that joins them.
        Icon::Magnet => {
            bar(items, -7.0, -3.0, 4.5, 8.0);
            bar(items, 2.5, -3.0, 4.5, 8.0);
            items.push(Item::Rect(RectItem { radius: 4.0, ..RectItem::new(RectPx { x: cx - 7.0, y: cy + 1.0, w: 14.0, h: 7.0 }, color) }));
            bar(items, -7.0, -8.0, 4.5, 3.0);
            bar(items, 2.5, -8.0, 4.5, 3.0);
        }
        Icon::Flag => {
            bar(items, -5.0, -8.0, 1.5, 16.0);
            bar(items, -3.5, -7.0, 9.5, 1.5);
            bar(items, -3.5, 0.0, 9.5, 1.5);
            bar(items, 4.5, -7.0, 1.5, 8.5);
        }
        Icon::Film => {
            items.push(Item::Rect(RectItem { radius: 1.5, border_w: 1.5, border_color: color,
                ..RectItem::new(RectPx { x: cx - 8.0, y: cy - 7.0, w: 16.0, h: 14.0 }, [0.0; 4]) }));
            bar(items, -4.5, -7.0, 1.5, 14.0);
            bar(items, 3.0, -7.0, 1.5, 14.0);
            for y in [-2.5, 2.0] {
                bar(items, -8.0, y, 4.0, 1.5);
                bar(items, 4.0, y, 4.0, 1.5);
            }
        }
        Icon::Speaker => {
            bar(items, -8.0, -3.0, 4.0, 6.0);
            items.push(Item::Triangle { r: RectPx { x: cx - 7.0, y: cy - 7.0, w: 8.0, h: 14.0 }, color, left: true, radius: 0.5 });
            bar(items, 3.5, -3.0, 1.5, 6.0);
            bar(items, 6.5, -5.5, 1.5, 11.0);
        }
        Icon::Caption => {
            items.push(Item::Rect(RectItem { radius: 1.5, border_w: 1.5, border_color: color,
                ..RectItem::new(RectPx { x: cx - 8.0, y: cy - 6.0, w: 16.0, h: 12.0 }, [0.0; 4]) }));
            bar(items, -5.0, -1.75, 3.5, 1.5);
            bar(items, 0.5, -1.75, 4.0, 1.5);
            bar(items, -5.0, 1.5, 6.0, 1.5);
        }
        Icon::Ticks => {
            for (i, h) in [6.0, 11.0, 6.0, 11.0, 6.0].into_iter().enumerate() {
                bar(items, -8.0 + i as f32 * 3.6, -h / 2.0, 1.5, h);
            }
        }
        // A frame: one point against a bar. A keyframe: two points. A chapter: a flag.
        Icon::StepBack | Icon::StepFwd => {
            let m = if matches!(icon, Icon::StepBack) { -1.0 } else { 1.0 };
            let at = |x: f32, w: f32| if m > 0.0 { x } else { -x - w };
            bar(items, at(4.5, 1.5), -5.5, 1.5, 11.0);
            items.push(Item::Triangle { r: RectPx { x: cx + at(-4.5, 8.0), y: cy - 5.5, w: 8.0, h: 11.0 }, color, left: m < 0.0, radius: 0.5 });
        }
        Icon::KeyBack | Icon::KeyFwd => {
            let m = if matches!(icon, Icon::KeyBack) { -1.0 } else { 1.0 };
            let at = |x: f32, w: f32| if m > 0.0 { x } else { -x - w };
            for x in [-7.5, -0.5] {
                items.push(Item::Triangle { r: RectPx { x: cx + at(x, 7.0), y: cy - 5.5, w: 7.0, h: 11.0 }, color, left: m < 0.0, radius: 0.5 });
            }
        }
        Icon::ChapBack | Icon::ChapFwd => {
            let m = if matches!(icon, Icon::ChapBack) { -1.0 } else { 1.0 };
            // A flag on the far side; a point leading toward it.
            draw_icon(items, Icon::Flag, cx + 3.5 * m, cy, color);
            let x = if m < 0.0 { cx - 9.5 } else { cx - 6.5 + 1.0 };
            items.push(Item::Triangle { r: RectPx { x, y: cy - 4.0, w: 5.0, h: 8.0 }, color, left: m < 0.0, radius: 0.5 });
        }
        Icon::List => {
            for y in [-5.5, -0.75, 4.0] { bar(items, -7.0, y, 14.0, 1.5); }
        }
        Icon::Grid => {
            for (x, y) in [(-6.5, -6.5), (0.5, -6.5), (-6.5, 0.5), (0.5, 0.5)] {
                items.push(Item::Rect(RectItem { radius: 1.0, border_w: 1.3, border_color: color, ..RectItem::new(RectPx { x: cx + x, y: cy + y, w: 6.0, h: 6.0 }, [0.0; 4]) }));
            }
        }
    }
}

/// The design's small-caps section label.
fn caps_label(items: &mut Vec<Item>, x: f32, y: f32, text: &str) {
    items.push(Item::Text(TextItem { valign: VAlign::Middle, tracking: 0.8,
        ..TextItem::new(x, y, 10.0, mix(CUT_BG, 0xffffff, 0.42), text) }));
}

/// `fg` at `alpha` over `bg`, composited in sRGB — the design's numbers.
/// The renderer blends in linear light, where the same alpha reads far
/// brighter, so translucent design tokens over a KNOWN ground are baked
/// to the opaque colour they were meant to produce.
fn mix(bg: u32, fg: u32, alpha: f32) -> [f32; 4] {
    let (b, f) = (hex_color(bg), hex_color(fg));
    [b[0] + (f[0] - b[0]) * alpha, b[1] + (f[1] - b[1]) * alpha, b[2] + (f[2] - b[2]) * alpha, 1.0]
}

/// Greedy word wrap by character count, for the one place the UI sets a
/// paragraph (the renderer has no wrapping; `max_width` still guards it).
fn wrap_text(text: &str, columns: usize) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for word in text.split_whitespace() {
        match lines.last_mut() {
            Some(line) if line.chars().count() + 1 + word.chars().count() <= columns => {
                line.push(' ');
                line.push_str(word);
            }
            _ => lines.push(word.to_string()),
        }
    }
    lines
}

const CUT_TOOLBAR_H: f32 = 50.0;
const CUT_TIMELINE_H: f32 = 222.0;
/// Short windows drop the chapter and audio rows.
const CUT_TIMELINE_COMPACT_H: f32 = 136.0;
const CUT_FULL_MIN_H: f32 = 640.0;
const CUT_SIDE_W: f32 = 300.0;
/// The icon gutter left of the rows, and the pad right of them.
const CUT_GUTTER: f32 = 56.0;
const CUT_PAD: f32 = 16.0;
/// How far from a clip's edge a press still takes hold of it.
const CUT_GRIP: f32 = 5.0;
// The Timeline Edit design's tokens, sRGB hex (its oklch values converted).
const CUT_BG: u32 = 0x0e0f10;
/// The lit tab / toggle ground: the app's TOOL_BG.
const CUT_TOOL_ON: u32 = 0x0b1926;
const CUT_TL: u32 = 0x0b0c0d;
/// The inspector's segment rows.
const CUT_LANE: u32 = 0x151719;
/// A kept piece of picture, and of sound (alternating, so neighbours part).
const CUT_FILM: u32 = 0x25282b;
const CUT_SOUND: u32 = 0x0c1d2e;
const CUT_SOUND_ALT: u32 = 0x0f2438;
const CUT_WAVE: u32 = 0x5db0f5;
/// In is blue, out is red; the cut button a lighter red.
const CUT_IN: u32 = 0x4aa3ee;
const CUT_OUT: u32 = 0xff6a72;
const CUT_HOT: u32 = 0xff8a90;
/// The playhead and the keyframes the selection snapped to.
const CUT_HEAD: u32 = 0xf2f3f4;
const CUT_INK: u32 = 0xededec;
/// Chapters and the keyframe chip.
const CUT_AMBER: u32 = 0xeaaa20;
const CUT_AMBER_INK: u32 = 0x1a1408;
/// Lossless / snapped.
const CUT_GREEN: u32 = 0x35c15f;
const CUT_GREEN_INK: u32 = 0x7fdc98;
/// Warnings only: long GOPs, what snapping costs, the cut button.
const CUT_CORAL: u32 = 0xf08566;
const CUT_CORAL_INK: u32 = 0xf5b3a0;
const CUT_CUE: u32 = 0xbfa8e8;

#[derive(Clone, Copy)]
struct Workspace {
    rail: f32,
    canvas: RectPx,
    list: RectPx,
    inspector: RectPx,
    transport: RectPx,
    /// Cut mode only (zero-sized otherwise): the lanes panel under the
    /// transport and the inspector column right of the canvas.
    timeline: RectPx,
    side: RectPx,
}
impl Workspace {
    fn new(vp: (f32, f32), visible: bool, cut: bool) -> Self {
        let (w, h) = vp;
        let none = RectPx { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };
        if visible && cut {
            // One clip: no source rail. Canvas and inspector share the top,
            // then the transport, then the lanes (compact on short windows).
            let lanes = if h >= CUT_FULL_MIN_H { CUT_TIMELINE_H } else { CUT_TIMELINE_COMPACT_H };
            let side = if w >= 1000.0 { CUT_SIDE_W } else { 0.0 };
            let transport_y = h - STATUS_H - lanes - CUT_TOOLBAR_H;
            let top = (transport_y - HEADER_H).max(1.0);
            return Self {
                rail: 0.0, list: none, inspector: none,
                canvas: RectPx { x: 0.0, y: HEADER_H, w: (w - side).max(1.0), h: top },
                side: RectPx { x: w - side, y: HEADER_H, w: side, h: top },
                transport: RectPx { x: 0.0, y: transport_y, w, h: CUT_TOOLBAR_H },
                timeline: RectPx { x: 0.0, y: transport_y + CUT_TOOLBAR_H, w, h: lanes },
            };
        }
        let rail = if !visible { 0.0 } else if w < 900.0 { 210.0_f32.min(w * 0.3) } else { 280.0 };
        let header = if visible { HEADER_H + CONTEXT_H } else { 0.0 };
        let bottom = if visible { TRANSPORT_H + STATUS_H } else { 0.0 };
        let canvas = RectPx { x: rail, y: header, w: (w - rail).max(1.0), h: (h - header - bottom).max(1.0) };
        let inspector_h = if visible && h >= 600.0 { 216.0 } else { 0.0 };
        let inspector = RectPx { x: 0.0, y: h - STATUS_H - inspector_h, w: rail, h: inspector_h };
        Self {
            rail, canvas, inspector, timeline: none, side: none,
            list: RectPx { x: 0.0, y: HEADER_H + 12.0, w: rail, h: (inspector.y - HEADER_H - 24.0).max(SOURCE_H) },
            transport: RectPx { x: rail, y: h - STATUS_H - TRANSPORT_H, w: w - rail, h: TRANSPORT_H },
        }
    }
}

#[derive(Clone, Copy)]
enum Action { Tool(usize), View(Mode), Brush(bool), Save, Export, Param(bool), Cut(CutAction) }
#[derive(Clone, Copy, PartialEq)]
enum CutAction { In, Out, Split, Apply, Undo, Export, Snap(Snap), Play, Zoom(bool), Step(i32), Keyframe(i32), Chapter(i32) }
/// `hint` is the key cap a cut-mode button carries ("" for none).
struct Control { r: RectPx, label: String, selected: bool, action: Action, hint: &'static str }
impl Control {
    fn new(r: RectPx, label: &str, selected: bool, action: Action) -> Self {
        Self { r, label: label.into(), selected, action, hint: "" }
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

    /// The marquee's middle label: a locked preset by name, a free rect by
    /// its reduced ratio while readable, else as n.nn:1.
    #[test]
    fn crop_ratio_label_names_presets_and_reduces_free_rects() {
        assert_eq!(ratio_label(640, 360, None), "16:9");
        assert_eq!(ratio_label(1080, 1080, None), "1:1");
        assert_eq!(ratio_label(3148, 2160, None), "1.46:1");
        assert_eq!(ratio_label(1918, 1080, Some("ratio 16:9")), "16:9");
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
    /// Cut mode end to end on a real clip: T opens the timeline on the
    /// focused clip, I/O snap to the scanned keyframes, X removes the
    /// range, playback jumps over it, ⌘Z restores it, and the shell's
    /// rectangles never overlap the canvas.
    /// The inspector: icon tabs, the list / thumbnail toggle, chapter rows
    /// that seek, and a removed clip that selects itself for X to restore.
    #[test]
    fn cut_inspector_tabs_toggle_chapters_seek_and_ghosts_select() {
        let Some(base) = test_clip() else { return };
        let dir = std::env::temp_dir().join("abner_app_test");
        let clip = dir.join("chapters.mp4");
        if !clip.exists() {
            let meta = dir.join("chapters.ffmeta");
            std::fs::write(&meta, ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=1500\ntitle=One\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=1500\nEND=4000\ntitle=Two\n").unwrap();
            let ok = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-i"]).arg(&base).arg("-i").arg(&meta)
                .args(["-map_metadata", "1", "-c", "copy"]).arg(&clip).status().map(|s| s.success()).unwrap_or(false);
            assert!(ok, "failed to generate the chaptered clip");
        }
        let mut app = mk_app(&clip, 1);
        assert!(tick_until(&mut app, Duration::from_secs(5), |a| a.started));
        app.key(Key::Char('t'));
        assert!(tick_until(&mut app, Duration::from_secs(10), |a| a.cut.as_ref().is_some_and(|c| !c.scanning())));
        app.key(Key::Space);
        assert_eq!(app.cut.as_ref().unwrap().chapters.len(), 2);
        assert!(!app.cut.as_ref().unwrap().facts.chips.is_empty(), "the stream chips never arrived");

        // Chapter rows seek to their chapter.
        assert_eq!(app.cut_tab, 0);
        let rows = app.cut_chapter_rows();
        assert_eq!(rows.len(), 2);
        let r = rows[1].0;
        app.mouse_down(r.x + 20.0, r.y + 10.0);
        app.mouse_up();
        assert!((app.t - 1.5).abs() < 0.1, "chapter two starts at 1.5 s, t = {}", app.t);

        // The skip buttons either side of play: chapters, keyframes, frames.
        let press = |app: &mut App, want: CutAction| {
            let r = app.controls().into_iter().find(|c| matches!(c.action, Action::Cut(a) if a == want)).expect("button").r;
            app.mouse_down(r.x + r.w / 2.0, r.y + r.h / 2.0);
            app.mouse_up();
        };
        app.seek_all(0.2, true);
        press(&mut app, CutAction::Chapter(1));
        assert!((app.t - 1.5).abs() < 0.05, "next chapter, t = {}", app.t);
        app.seek_all(1.6, true);
        press(&mut app, CutAction::Chapter(-1));
        assert!(app.t < 0.05, "just inside chapter two, back goes to chapter one, t = {}", app.t);
        press(&mut app, CutAction::Keyframe(1));
        assert!((app.t - 1.0).abs() < 0.05, "next keyframe, t = {}", app.t);
        press(&mut app, CutAction::Keyframe(-1));
        assert!(app.t < 0.05, "previous keyframe, t = {}", app.t);
        assert!(!app.playing);

        // `+` starts a chapter at the playhead; a second press there is refused.
        app.seek_all(3.0, true);
        let side = app.cut_side();
        app.mouse_down(side.add_btn.x + 4.0, side.add_btn.y + 4.0);
        app.mouse_up();
        assert_eq!(app.cut.as_ref().unwrap().chapters.len(), 3);
        assert_eq!(app.cut.as_ref().unwrap().chapters[2].start, 3.0);
        app.mouse_down(side.add_btn.x + 4.0, side.add_btn.y + 4.0);
        app.mouse_up();
        assert_eq!(app.cut.as_ref().unwrap().chapters.len(), 3, "no second chapter on the same spot");

        // The toggle swaps the list for thumbnail cards (two columns).
        let side = app.cut_side();
        app.mouse_down(side.grid_btn.x + 4.0, side.grid_btn.y + 4.0);
        app.mouse_up();
        assert!(app.cut_thumbs);
        let cards = app.cut_chapter_rows();
        assert_eq!(cards.len(), 3);
        assert!(cards[0].0.y == cards[1].0.y && cards[1].0.x > cards[0].0.x, "cards sit side by side");
        app.mouse_down(side.list_btn.x + 4.0, side.list_btn.y + 4.0);
        app.mouse_up();
        assert!(!app.cut_thumbs);

        // The streams tab replaces the chapter rows.
        let side = app.cut_side();
        app.mouse_down(side.tab[1].x + 4.0, side.tab[1].y + 4.0);
        app.mouse_up();
        assert_eq!(app.cut_tab, 1);
        assert!(app.cut_chapter_rows().is_empty());
        let side = app.cut_side();
        app.mouse_down(side.tab[0].x + 4.0, side.tab[0].y + 4.0);
        app.mouse_up();
        assert_eq!(app.cut_tab, 0);

        // A removed clip selects itself on a click, ready for X to restore it.
        app.seek_all(1.4, true);
        app.key(Key::Char('i'));
        app.seek_all(1.7, true);
        app.key(Key::Char('o'));
        app.key(Key::Char('x'));
        assert!(app.cut.as_ref().unwrap().selection().is_none());
        let v = app.cut_lanes().video;
        app.mouse_down(v.x + v.w * 0.375, v.y + 10.0);
        app.mouse_up();
        assert_eq!(app.cut.as_ref().unwrap().selection(), Some((1.0, 2.0)));
    }

    /// U undoes like ⌘Z, and the wheel over the timeline zooms about the pointer
    /// (up in, down out) while a sideways swipe pans.
    #[test]
    fn cut_mode_u_undoes_and_the_wheel_zooms_the_track() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 1);
        assert!(tick_until(&mut app, Duration::from_secs(5), |a| a.started));
        app.key(Key::Char('t'));
        assert!(tick_until(&mut app, Duration::from_secs(10), |a| a.cut.as_ref().is_some_and(|c| !c.scanning())));
        app.key(Key::Space);
        app.seek_all(1.4, true);
        app.key(Key::Char('i'));
        app.seek_all(1.7, true);
        app.key(Key::Char('o'));
        app.key(Key::Char('x'));
        assert_eq!(app.cut.as_ref().unwrap().cuts().len(), 1);
        app.key(Key::Char('u'));
        assert!(app.cut.as_ref().unwrap().cuts().is_empty(), "U should undo the cut");

        let v = app.cut_lanes().video;
        app.cursor_moved(v.x + v.w * 0.5, v.y + 10.0);
        let before = app.cut.as_ref().unwrap().pps;
        app.scroll(0.0, -20.0);
        let zoomed = app.cut.as_ref().unwrap().pps;
        assert!(zoomed > before, "scrolling up should zoom in: {before} -> {zoomed}");
        app.scroll(0.0, 20.0);
        assert!(app.cut.as_ref().unwrap().pps < zoomed, "scrolling down should zoom back out");
    }

    #[test]
    fn cut_mode_snaps_cuts_skips_and_undoes() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 1);
        assert!(tick_until(&mut app, Duration::from_secs(5), |a| a.started));
        app.key(Key::Char('t'));
        assert!(app.cut_mode);
        // The 4 s clip has a keyframe every second (-g 30 at 30 fps).
        assert!(tick_until(&mut app, Duration::from_secs(10), |a| a.cut.as_ref().is_some_and(|c| !c.scanning())));
        assert_eq!(app.cut.as_ref().unwrap().keys.len(), 4);
        let l = app.workspace();
        assert_eq!(l.canvas.y + l.canvas.h, l.transport.y);
        assert_eq!(l.transport.y + l.transport.h, l.timeline.y);
        assert_eq!(l.canvas.x + l.canvas.w, l.side.x);
        let lanes = app.cut_lanes();
        assert!(lanes.keys.y + lanes.keys.h <= l.timeline.y + l.timeline.h);

        app.key(Key::Space);
        assert!(!app.playing);
        app.seek_all(1.4, true);
        app.key(Key::Char('i'));
        app.seek_all(1.7, true);
        app.key(Key::Char('o'));
        assert_eq!(app.cut.as_ref().unwrap().selection(), Some((1.0, 2.0)));
        app.key(Key::Char('x'));
        assert_eq!(app.cut.as_ref().unwrap().cuts(), [(1.0, 2.0)]);
        assert_eq!(app.cut.as_ref().unwrap().segments().len(), 3);

        // ⇧→ lands on the next keyframe, exactly.
        app.seek_all(2.2, true);
        app.key(Key::KeyRight);
        assert_eq!(app.t, 3.0);

        // Playing into the cut resumes at its end.
        app.seek_all(0.9, true);
        app.playing = true;
        assert!(tick_until(&mut app, Duration::from_secs(5), |a| a.t >= 2.0));
        assert!(app.t < 2.5, "jumped the cut rather than playing through it: {}", app.t);

        // A press on the lanes scrubs; the header's Undo button undoes.
        let v = app.cut_lanes().video;
        app.mouse_down(v.x + v.w * 0.75, v.y + 10.0);
        app.mouse_up();
        assert!((app.t - 3.0).abs() < 0.1, "{}", app.t);
        app.key(Key::Undo);
        assert!(app.cut.as_ref().unwrap().cuts().is_empty());

        // Leaving and coming back keeps the model; closing the clip drops it.
        app.key(Key::Char('x'));
        app.key(Key::Escape);
        assert!(!app.cut_mode && app.cut.is_some());
        app.key(Key::Close);
        assert!(app.cut.is_none());
    }

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
/// The recent row (the Splash design's recent files): 16:9 tiles on the
/// 4px grid, spaced well apart, `radius-8`.
const TILE_W: f32 = 132.0;
const TILE_H: f32 = 74.0;
const TILE_GAP: f32 = 64.0;
const TILE_R: f32 = 8.0;
/// Header line, the step between the row's lines, the metadata line.
const RECENT_HEAD: f32 = 14.0;
const RECENT_STEP: f32 = 8.0;
const RECENT_META: f32 = 16.0;
/// Floor kept clear above the row (under the horizon) and below it
/// (over the foot).
const RECENT_CLEAR: f32 = 16.0;
/// The tile's recessed ground (the brand's `well`, #04050a).
const WELL: [f32; 4] = [0.016, 0.020, 0.039, 1.0];
/// The bevel: white hairlines. The design's sRGB alphas (0.16 all round,
/// 0.34 on top) are run lower here because blending is linear-space.
const BEVEL: [f32; 4] = [1.0, 1.0, 1.0, 0.09];
const BEVEL_LIT: [f32; 4] = [1.0, 1.0, 1.0, 0.20];
/// The corner chips: `void` at 0.85 under `ink` (#e8edf2).
const CHIP_BG: [f32; 4] = [0.016, 0.020, 0.039, 0.85];
const CHIP_INK: [f32; 4] = [0.910, 0.929, 0.949, 1.0];
/// The brand's `ink-faint` (#7a8694): the key hint beside the header.
/// Version, git hash (`+dirty` with uncommitted edits) and build time, stamped by
/// build.rs. The launch window prints it so a screenshot says which build it is.
const BUILD: &str = env!("ABNER_BUILD");

fn build_label(w: f32, h: f32) -> Item {
    Item::Text(TextItem {
        align: Align::Right,
        valign: VAlign::Middle,
        tracking: 0.4,
        ..TextItem::new(w - 16.0, h - 14.0, 9.5, INK_FAINT, format!("build {BUILD}"))
    })
}

const INK_FAINT: [f32; 4] = [0.478, 0.525, 0.580, 1.0];

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
/// The marquee's label chips (Figma: white @0.12, flat, 10px #d1d1d1). The
/// alpha is the linear-space equivalent of the design's sRGB 0.12 — at 0.12
/// here a white wash reads several times brighter than in the mock.
const CROP_CHIP: [f32; 4] = [0.0, 0.0, 0.0, 0.8];
/// The guide lines running out of each marquee side across the canvas.
const CROP_GUIDE: [f32; 4] = [1.0, 1.0, 1.0, 0.22];
const DOUBLE_CLICK: std::time::Duration = std::time::Duration::from_millis(350);
const CROP_CHIP_PAD: (f32, f32) = (10.0, 3.0);
/// Screen size the marquee needs before the corner coordinates show, and
/// before the size/ratio label in the middle does.
const CROP_LABELS_MIN: (f32, f32) = (180.0, 72.0);
const CROP_LABEL_MIN: (f32, f32) = (130.0, 24.0);


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

/// The ratio a crop is shown with: a locked preset by its own name, a free
/// rect as its reduced ratio while that stays readable (640×360 → 16:9),
/// otherwise as `n.nn:1`.
fn ratio_label(w: u32, h: u32, preset: Option<&str>) -> String {
    if let Some(name) = preset {
        return name.trim_start_matches("ratio ").to_string();
    }
    let gcd = |mut a: u32, mut b: u32| { while b != 0 { (a, b) = (b, a % b); } a.max(1) };
    let g = gcd(w, h);
    let (rw, rh) = (w / g, h / g);
    if rw <= 32 && rh <= 32 { format!("{rw}:{rh}") } else { format!("{:.2}:1", w as f32 / h.max(1) as f32) }
}

fn name_of(path: &std::path::Path) -> String {
    path.file_name().unwrap_or_default().to_string_lossy().into_owned()
}

fn fmt_time(t: f64) -> String {
    let t = t.max(0.0);
    let m = (t / 60.0) as u64;
    format!("{m:02}:{:06.3}", t - m as f64 * 60.0)
}

/// `HH:MM:SS:FF`, non-drop: the frame count over the rounded rate.
fn fmt_tc(t: f64, fps: f64) -> String {
    let base = fps.round().max(1.0) as u64;
    let frames = (t.max(0.0) * fps).round() as u64;
    let (s, f) = (frames / base, frames % base);
    format!("{:02}:{:02}:{:02}:{f:02}", s / 3600, s / 60 % 60, s % 60)
}

fn fmt_hms(t: f64) -> String {
    let s = t.max(0.0) as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

/// A length, as short as it reads: `0:20`, `37:38`, `1:02:10`.
fn fmt_span(t: f64) -> String {
    let s = t.max(0.0).round() as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
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
