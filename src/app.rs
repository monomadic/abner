//! App state and per-frame logic: the master clock, view modes, input,
//! and the UI overlay.
//!
//! Sync model: one master time `t` advances by wall-clock dt while
//! playing; every player queues `(pts, rgba)` frames and each frame the
//! app pops everything `pts <= t` (newest wins). All streams answer to
//! the same clock, so switching the displayed video (Tab) can never
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
    /// Videos stacked on top of each other; Tab flips which one shows.
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
    DuplicateSources,
    SelectSources,
    OpenFiles,
    /// ⌘ plus a digit: on the launch window, open that recent file; with
    /// clips up it is the bare digit (pick that clip), as it always was.
    CmdDigit(char),
    /// ⌘Z: cut mode's undo.
    Undo,
    /// ⌘R: render pending split GOPs in the timeline.
    Render,
    /// ⇧← / ⇧→: cut mode's previous / next keyframe. Plain arrows elsewhere.
    KeyLeft,
    KeyRight,
}

#[derive(Debug, Clone)]
pub enum Cmd {
    Quit,
    ToggleFullscreen,
    /// The clip list changed shape outside a drop (⌘W): the runner
    /// re-syncs the GPU's per-slot textures and the window title.
    VideosChanged,
    /// Open the launch window's nth recent tile (a click or ⌘1–4). The
    /// runner loads it through the drop path, so it is recorded again.
    OpenRecent(usize),
    OpenFiles,
    OpenDirectory(std::path::PathBuf),
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
    /// A rubber band from a press on a vacant part of the lanes: every kept
    /// clip (or GOP) it touches is picked. `add` = ⌘ held, so the picks it
    /// started with (`cut_band_base`) stay in.
    Band { x0: f32, y0: f32, add: bool },
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

/// How long a seek may stay "in flight" before the app stops waiting on
/// it (see `Video::seek_sent`).
const SEEK_SETTLE: std::time::Duration = std::time::Duration::from_secs(2);

pub struct Video {
    /// Batch scope is independent of the source currently shown in the viewer.
    pub selected: bool,
    /// An independent item referring to existing media; never a filesystem copy.
    pub linked_copy: bool,
    pub info: VideoInfo,
    pub player: Player,
    pub shown_pts: f64,
    pub delivered: bool,
    /// Waiting to adopt the first frame after an exact seek while paused.
    pub pending: bool,
    /// A seek was sent and nothing has landed from it yet. While any
    /// player is in this state `seek_all` holds the newest target back
    /// (`App::queued_seek`) instead of restarting the reader — otherwise a
    /// scrub sends a seek per pointer event, each one abandoning the
    /// previous GOP decode, and nothing ever reaches the screen.
    pub seeking: bool,
    /// When the seek in flight was sent. A seek past a stream's last frame
    /// never delivers (the reader discards to EOF and parks), so in-flight
    /// expires after `SEEK_SETTLE` rather than wedging every later seek.
    pub seek_sent: std::time::Instant,
    /// The frame on screen, kept only while mask mode is on: a crop export
    /// writes these pixels, and the GPU's copy can't be read back. Mask
    /// mode is paused, so this costs one copy per seek, not per frame.
    pub last_frame: Option<std::sync::Arc<Vec<u8>>>,
}

pub struct App {
    pub videos: Vec<Video>,
    active: usize,
    source_scroll: usize,
    source_copy_drag: Option<(usize, f32, f32, bool)>,
    source_status: String,
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
    cut_video: usize,
    cut_mode: bool,
    /// A lone clip opens on the timeline (main.rs turns this on; tests leave
    /// it off so Sources stays the layout they exercise).
    cut_default: bool,
    /// The inspector's tab (0 chapters, 1 streams) and its chapter view.
    cut_tab: usize,
    /// Only discrete seeks/framesteps adopt a decoded timestamp; pointer seeks stay pinned.
    adopt_seek: bool,
    /// The seek that arrived while one was in flight: `(target, exact)`,
    /// newest wins, dispatched by `tick` once every player has landed.
    queued_seek: Option<(f64, bool)>,
    info_scroll: f32,
    cut_frames: bool,
    frame_changes: bool,
    cut_gop: bool,
    frame_detail_boot: bool,
    cut_thumbs: bool,
    cut_posters: bool,
    /// The start-of-clip thumbnails on the picture lane (posters, and the one under the pointer playing).
    cut_thumb: CutThumbs,
    split_preview: Option<SplitPreview>,
    /// The picture clip last clicked, by its (start, end): drawn outlined.
    /// The picked clips (or GOPs, `cut_gop`): (start, end, sound row). A
    /// plain click replaces the set, ⌘-click toggles one, a rubber band
    /// sweeps many; the last entry is the most recent pick.
    cut_picks: Vec<(f64, f64, bool)>,
    /// The picks a ⌘-band started from.
    cut_band_base: Vec<(f64, f64, bool)>,
    /// ⌘ was down at the press that `mouse_down` is handling.
    press_cmd: bool,
    cut_restore: Option<(f64, f64)>,
    split_tool: bool,
    /// Hover lift per kept clip, keyed by (start ms, sound lane): 0..1, eased
    /// up while the pointer is over it and back down after it leaves.
    cut_hot: Vec<(i64, bool, f32)>,
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
    /// Per-clip decoded-frame cache budget (`playback.frame_cache_mb`).
    cache_bytes: usize,
    /// Rebindable keys (config `keys`).
    keys: crate::config::Keys,
    /// Loaded clips open paused on their first frame (config
    /// `playback.start_paused`) — at startup AND on every later drop/open.
    start_paused: bool,
    show_ui: bool,
    /// The Sources sidebar (Figma Abner / Toolbar / Sidebar toggle): the
    /// Finder-style button in the header hides the rail in every mode that
    /// has one. Remembered across mode switches; opening a clip never
    /// forces a hidden rail open.
    show_sources: bool,
    /// How far the rail is in, 0..1: the design's 200 ms slide
    /// (`RAIL_SLIDE_S`) toward `show_sources`. The panel's width in the
    /// layout follows it; the panel's CONTENT keeps its full width and
    /// slides left, so nothing squashes.
    rail_slide: f32,
    /// The timeline's inspector (the right toggle of the same component):
    /// independent of the rail, same 200 ms slide, this time off the
    /// right edge.
    show_inspector: bool,
    side_slide: f32,
    /// The rail's focused inspector (the detail panel under the cards):
    /// the grip strip along its top edge folds it down behind the action
    /// row and back, over `DETAIL_SLIDE_S`. The grip stays, so it can be
    /// reopened.
    show_detail: bool,
    detail_slide: f32,
    /// Delta amplification.
    gain: f32,
    blend: f32,
    checker_px: f32,
    /// Seconds left on the small clip-number flash (shown after Tab
    /// when the UI is hidden — switchblade's skip-bar-flash pattern).
    badge_flash: f32,
    fullscreen: bool,
    cursor: (f32, f32),
    /// Last pointer position while a drag-pan is held.
    drag: Option<(f32, f32)>,
    /// Dragging the seek bar (pins the transport open).
    scrubbing: bool,
    /// Playback was running when a timeline scrub took hold; it resumes on release.
    scrub_resume: bool,
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
/// The board's chapter flag, shared by the timeline and the inspector list: square
/// on the left (where the pole is), a pill on the right — a D — with the number
/// centred in it, black on amber when lit. Returns its width.
fn chapter_flag(items: &mut Vec<Item>, x: f32, y: f32, h: f32, number: String, lit: bool, past: bool) -> f32 {
    let w = (number.chars().count() as f32 * 10.5 * MONO_ADV + 12.0).max(20.0).round();
    let (bg, fg) = if lit { (hex_color(CUT_AMBER), hex_color(0x000000)) }
        else if past { (hex_color(CUT_FLAG_PAST), hex_color(CUT_INK)) }
        else { (mix(CUT_TL, 0xffffff, 0.16), hex_color(CUT_INK)) };
    let flag = RectPx { x, y, w, h };
    items.push(Item::Rect(RectItem { radius: h / 2.0, ..RectItem::new(flag, bg) }));
    items.push(Item::Rect(RectItem { radius: 3.0, ..RectItem::new(RectPx { w: 12.0, ..flag }, bg) }));
    // The board sets the number in Bold; the renderer has one weight, so the
    // run is drawn twice half a pixel apart — a thin black stroke alone
    // anti-aliases into the amber and reads brown.
    for dx in [-0.3, 0.3] {
        ui_label(items, x + w / 2.0 + dx, y + h / 2.0, 10.5, fg, number.clone(), Align::Center, w);
    }
    w
}

/// One poster in the recent atlas, keyed by its clip's start in ms.
struct ThumbCell {
    key: i64,
    rgba: Vec<u8>,
}

/// The thumbnail under the pointer, playing its clip's range on its own
/// decoder and clock — independent of the master clock.
struct ThumbPlay {
    key: i64,
    slot: usize,
    player: Player,
    start: f64,
    end: f64,
    fps: f64,
    t: f64,
}

type ThumbMsg = (std::path::PathBuf, i64, Result<Vec<u8>, String>);

/// A separate native-resolution decoder: split inspection never moves the clock.
struct SplitPreview {
    path: std::path::PathBuf,
    idx: usize,
    player: Player,
    target: f64,
    waiting: bool,
    shown: bool,
    shown_pts: f64,
}

/// Cut mode's clip thumbnails. The recent row's atlas is free once a clip is
/// loaded, so its cells hold the posters; hovering one plays it in place and
/// leaving restores the poster.
struct CutThumbs {
    cells: Vec<ThumbCell>,
    path: Option<std::path::PathBuf>,
    pending: Vec<i64>,
    failed: Vec<i64>,
    tx: std::sync::mpsc::Sender<ThumbMsg>,
    rx: std::sync::mpsc::Receiver<ThumbMsg>,
    play: Option<ThumbPlay>,
    hover: Option<i64>,
    uploads: Vec<ThumbUpload>,
}

impl CutThumbs {
    fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        Self { cells: Vec::new(), path: None, pending: Vec::new(), failed: Vec::new(), tx, rx, play: None, hover: None, uploads: Vec::new() }
    }

    fn slot_of(&self, key: i64) -> Option<usize> {
        self.cells.iter().position(|c| c.key == key)
    }

    fn reset(&mut self) {
        let (tx, rx) = std::sync::mpsc::channel(); self.tx = tx; self.rx = rx;
        self.uploads.clear();
        self.cells.clear();
        self.pending.clear();
        self.failed.clear();
        self.play = None;
        self.hover = None;
    }

    /// Put a slot's poster back after its playback ended.
    fn restore(&mut self, slot: usize) {
        if let Some(c) = self.cells.get(slot) {
            self.uploads.push(ThumbUpload { slot, buf: c.rgba.clone() });
        }
    }
}

/// A decoded `w × h` frame as one atlas cell: centre-cropped when taller than
/// 16:9, padded black when shorter.
fn fit_cell(buf: &[u8], w: u32, h: u32) -> Vec<u8> {
    let (cw, ch) = (recent::THUMB_W as usize, recent::THUMB_H as usize);
    let (w, h) = (w as usize, h as usize);
    let mut out = vec![0u8; cw * ch * 4];
    for px in out.chunks_exact_mut(4) { px[3] = 255; }
    if w != cw || buf.len() < w * h * 4 { return out; }
    let shift = (h as isize - ch as isize) / 2;
    for y in 0..ch {
        let sy = y as isize + shift;
        if sy < 0 || sy >= h as isize { continue; }
        let sy = sy as usize;
        out[y * cw * 4..(y + 1) * cw * 4].copy_from_slice(&buf[sy * w * 4..(sy * w + cw) * 4]);
    }
    out
}

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
            cut_video: 0,
            cut_mode: false,
            cut_default: false,
            cut_drag: None,
            cut_tab: 0,
            adopt_seek: false,
            queued_seek: None,
            info_scroll: 0.0,
            cut_frames: false,
            frame_changes: false, cut_gop: false, frame_detail_boot: false,
            cut_thumbs: false,
            cut_posters: true,
            cut_thumb: CutThumbs::new(),
            split_preview: None,
            cut_picks: Vec::new(),
            cut_band_base: Vec::new(),
            press_cmd: false,
            cut_restore: None,
            split_tool: false,
            cut_hot: Vec::new(),
            cut_last_t: f64::NAN,
            cut_boot: Vec::new(),
            videos,
            active: 0,
            source_scroll: 0,
            source_copy_drag: None,
            source_status: String::new(),
            t: 0.0,
            playing: !cfg.playback.start_paused,
            started: false,
            mode: cfg.playback.view,
            zoom: 1.0,
            center: (0.5, 0.5),
            speed: 1.0,
            seek_step: cfg.playback.seek_step,
            cache_bytes: cfg.frame_cache_bytes(),
            keys: cfg.keys.clone(),
            start_paused: cfg.playback.start_paused,
            show_ui: true,
            show_sources: true,
            rail_slide: 1.0,
            show_inspector: true,
            side_slide: 1.0,
            show_detail: true,
            detail_slide: 1.0,
            gain: cfg.compare.delta_gain,
            blend: cfg.compare.blend,
            checker_px: cfg.compare.checker_size,
            badge_flash: 0.0,
            fullscreen: false,
            cursor: (0.0, 0.0),
            drag: None,
            scrubbing: false,
            scrub_resume: false,
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
        if self.inspector_path_hit(self.cursor.0, self.cursor.1) {
            return Some(CropCursor::Pointer);
        }
        if self.show_ui && self.cursor_inside && contains(self.workspace().grip, self.cursor.0, self.cursor.1) {
            return Some(CropCursor::Pointer);
        }
        if self.cut_mode && self.show_ui && self.cursor_inside {
            if self.split_tool && self.cut_clip_at(self.cursor.0, self.cursor.1).is_some() { return Some(CropCursor::Pointer); }
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
                crate::player::SMALL_CACHE,
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
    /// Open a lone clip on the timeline — at launch, and for each drop that
    /// leaves exactly one clip loaded. Two or more clips stay in Sources.
    pub fn set_cut_default(&mut self, on: bool) {
        self.cut_default = on;
        self.settle_cut_default();
    }

    fn settle_cut_default(&mut self) {
        if self.cut_default && self.videos.len() == 1 && !self.cut_mode {
            self.set_cut_mode(true);
        }
    }

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
        self.source_copy_drag = None;
        if videos.is_empty() {
            return;
        }
        if replace {
            self.videos.clear();
            self.masks.clear();
        }
        self.cut = None;
        self.cut_restore = None;
        self.cut_mode = false;
        self.source_status.clear();
        self.source_scroll = 0;
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
        self.playing = !self.start_paused;
        self.active = 0;
        self.speed = 1.0;
        self.zoom = 1.0;
        self.center = (0.5, 0.5);
        self.drag = None;
        self.scrubbing = false;
        self.drag_hover = false;
        self.settle_cut_default();
    }

    /// Duplicate selected source items (or the viewed source when none are checked).
    /// Media stays at its existing path; each item gets independent playback/mask state.
    /// Do not use add_videos: duplication preserves the master time and playback state.
    pub fn duplicate_sources(&mut self) {
        if !self.ready() || self.cut_mode || self.mask_mode { return; }
        let mut indices: Vec<usize> = self.videos.iter().enumerate()
            .filter_map(|(i, v)| v.selected.then_some(i)).collect();
        if indices.is_empty() { indices.push(self.active); }
        self.duplicate_source_indices(&indices, None);
    }

    fn duplicate_source_indices(&mut self, indices: &[usize], destination: Option<usize>) {
        let mut copies = Vec::new();
        for &i in indices {
            let info = self.videos[i].info.clone();
            let Some(player) = Player::spawn(&info.path, info.width, info.height,
                crate::probe::vt_accel(&info.codec), info.rotation, self.cache_bytes) else {
                self.source_status = "Could not duplicate source: decoder unavailable".into();
                return;
            };
            copies.push((i, Video { selected: true, linked_copy: true, info, player,
                shown_pts: 0.0, delivered: false, pending: false, seeking: false, seek_sent: std::time::Instant::now(), last_frame: None }));
        }
        for v in &mut self.videos { v.selected = false; }
        let focus = destination.unwrap_or(indices[0] + 1);
        for (i, v) in copies.into_iter().rev() {
            let at = destination.unwrap_or(i + 1);
            self.videos.insert(at, v);
            self.masks.insert(at, None);
        }
        self.cut = None;
        self.cut_restore = None;
        // Insertion changes texture slots, including for paused survivors.
        for v in &mut self.videos { v.delivered = false; }
        self.seek_all(self.t, true);
        self.select(focus);
        self.source_status.clear();
        self.cmds.push(Cmd::VideosChanged);
    }

    fn source_file_count(&self) -> usize {
        self.videos.iter().map(|v| &v.info.path).collect::<std::collections::HashSet<_>>().len()
    }

    /// Drop the focused clip (⌘W). The survivors shift down a slot, and
    /// the GPU's textures are indexed by slot, so every survivor is
    /// exact-seeked to the current `t`: each re-delivers the frame for
    /// this moment into its new slot (a paused one too, via `pending`)
    /// rather than showing its neighbour's last frame. Closing the last
    /// clip returns to the launch window.
    fn close_active(&mut self) {
        self.close_sources(false);
    }

    fn close_sources(&mut self, selected: bool) {
        if self.videos.is_empty() {
            return;
        }
        let mut indices: Vec<usize> = self.videos.iter().enumerate()
            .filter_map(|(i, v)| (selected && v.selected).then_some(i)).collect();
        if indices.is_empty() { indices.push(self.active.min(self.videos.len() - 1)); }
        let idx = self.active - indices.iter().filter(|&&i| i < self.active).count();
        for &i in indices.iter().rev() {
            self.videos.remove(i);
            self.masks.remove(i);
        }
        self.cut = None;
        self.cut_restore = None;
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
        self.reveal_source();
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

    /// The per-clip decoded-frame cache budget, for players spawned
    /// outside `App` (main's drop/open loaders).
    pub fn cache_bytes(&self) -> usize {
        self.cache_bytes
    }

    /// Seek every stream. The clock moves at once; the players are told
    /// now, or — when one is still landing an earlier seek — once it has,
    /// so a burst of targets costs one restart, not one per target.
    fn seek_all(&mut self, target: f64, exact: bool) {
        self.adopt_seek = true;
        self.t = target;
        if self.seek_in_flight() {
            self.queued_seek = Some((target, exact));
        } else {
            self.dispatch_seek(target, exact);
        }
    }

    fn seek_in_flight(&self) -> bool {
        self.videos.iter().any(|v| v.seeking && !v.player.failed() && v.seek_sent.elapsed() < SEEK_SETTLE)
    }

    fn dispatch_seek(&mut self, target: f64, exact: bool) {
        self.queued_seek = None;
        let paused = !self.playing;
        let now = std::time::Instant::now();
        for v in &mut self.videos {
            v.player.seek(target, exact);
            v.seeking = true;
            v.seek_sent = now;
            if paused {
                v.pending = true;
            }
        }
    }

    /// Snap a pointer time to the frame grid, so a pixel's worth of jitter
    /// that names the same frame doesn't become a fresh seek.
    fn quantize(&self, t: f64) -> f64 {
        let d = 1.0 / self.fps.max(1.0);
        (t / d).round() * d
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
        if k == Key::OpenFiles { self.cmds.push(Cmd::OpenFiles); return; }
        if k == Key::DuplicateSources { self.duplicate_sources(); return; }
        if k == Key::SelectSources {
            if !self.cut_mode && !self.mask_mode {
                let select = !self.videos.iter().all(|v| v.selected);
                for v in &mut self.videos { v.selected = select; }
            }
            return;
        }
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
        if k == Key::Backspace && !self.cut_mode && !self.mask_mode {
            self.close_sources(true);
            return;
        }
        if k == Key::Close {
            self.close_active();
            return;
        }
        if k == self.keys.fullscreen_video {
            self.toggle_video_fullscreen();
            return;
        }
        if k == self.keys.next_source {
            // Cut mode holds one clip: nothing to flip to.
            if !self.cut_mode { self.select((self.active + 1) % self.videos.len()); }
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
            Key::Space => self.playing = !self.playing,
            Key::Enter | Key::Tab => {} // bindable, handled above
            Key::Left => self.seek_by(-self.seek_step),
            Key::Right => self.seek_by(self.seek_step),
            Key::Escape => {
                if self.fullscreen {
                    self.fullscreen = false;
                    self.show_ui = true;
                    self.cmds.push(Cmd::ToggleFullscreen);
                } else {
                    self.cmds.push(Cmd::Quit);
                }
            }
            Key::Backspace => self.speed = 1.0,
            Key::Close | Key::CmdDigit(_) | Key::Undo | Key::Render | Key::KeyLeft | Key::KeyRight | Key::DuplicateSources | Key::SelectSources | Key::OpenFiles => {} // handled above
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
                'x' if !self.mask_mode && !self.cut_mode => self.videos[self.active].selected ^= true,
                _ => {}
            },
        }
    }

    /// The current video alone: shell hidden and the window fullscreen, or
    /// back to the workspace if that's where we already are.
    fn toggle_video_fullscreen(&mut self) {
        self.mouse_up();
        let on = !(self.fullscreen && !self.show_ui);
        self.show_ui = !on;
        if self.fullscreen != on {
            self.fullscreen = on;
            self.cmds.push(Cmd::ToggleFullscreen);
        }
    }

    /// Show clip `idx` (`keys.next_source`'s flip, or its number key directly).
    fn select(&mut self, idx: usize) {
        let reopen_cut = self.cut_mode && self.active != idx;
        self.active = idx;
        if reopen_cut { self.cut_mode = false; self.set_cut_mode(true); }
        self.reveal_source();
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

    fn reveal_source(&mut self) {
        let capacity = self.source_capacity();
        if self.active < self.source_first() { self.source_scroll = self.active; }
        else if self.active >= self.source_first() + capacity {
            self.source_scroll = self.active + 1 - capacity;
        }
    }

    pub fn cursor_moved(&mut self, x: f32, y: f32) {
        self.cursor_inside = true;
        if let Some((_, sx, sy, moved)) = &mut self.source_copy_drag {
            *moved |= (x - *sx).hypot(y - *sy) >= 5.0;
            self.cursor = (x, y);
            return;
        }
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
            // The band reads the pointer's y from `cursor`.
            self.cursor = (x, y);
            self.cut_drag_to(x);
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

    pub fn mouse_down_with_alt(&mut self, x: f32, y: f32, alt: bool) {
        self.mouse_down_with_mods(x, y, alt, false);
    }

    /// `cmd` = ⌘ held: on the timeline it adds a clip to the picks instead
    /// of replacing them, and keeps a rubber band's starting picks.
    pub fn mouse_down_with_mods(&mut self, x: f32, y: f32, alt: bool, cmd: bool) {
        self.press_cmd = cmd;
        if alt && self.show_ui && !self.cut_mode && !self.mask_mode && contains(self.workspace().list, x, y) {
            let first = self.source_first();
            if let Some(idx) = (first..(first + self.source_capacity()).min(self.videos.len()))
                .find(|&i| contains(self.source_row(i), x, y) && !contains(self.source_checkbox(i), x, y)) {
                self.mouse_up();
                self.cursor = (x, y);
                self.cursor_inside = true;
                self.source_copy_drag = Some((idx, x, y, false));
                return;
            }
        }
        self.mouse_down(x, y);
        self.press_cmd = false;
    }

    pub fn mouse_released(&mut self) {
        let copy = self.source_copy_drag.take();
        self.mouse_up();
        if let Some((idx, _, _, true)) = copy {
            if self.cursor_inside && self.show_ui && !self.cut_mode && !self.mask_mode
                && contains(self.workspace().list, self.cursor.0, self.cursor.1) && idx < self.videos.len() {
                let at = self.source_drop_index(self.cursor.1);
                self.duplicate_source_indices(&[idx], Some(at));
            }
        }
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
        if self.inspector_path_hit(x, y) {
            if let Some(dir) = self.videos[self.active].info.path.parent() {
                self.cmds.push(Cmd::OpenDirectory(dir.to_path_buf()));
            }
            return;
        }
        if self.show_ui {
            if let Some(control) = self.controls().into_iter().find(|c| contains(c.r, x, y)) {
                self.activate(control.action);
                return;
            }
            if contains(self.workspace().list, x, y) {
                let first = self.source_first();
                for idx in first..(first + self.source_capacity()).min(self.videos.len()) {
                    if contains(self.source_row(idx), x, y) {
                        if self.batch_ui() && contains(self.source_checkbox(idx), x, y) {
                            self.videos[idx].selected ^= true;
                        } else { self.select(idx); }
                        break;
                    }
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

    /// A press on the timeline takes the playhead and holds it: the clock
    /// stops while the button is down, and `mouse_up` lets it run again.
    fn begin_cut_scrub(&mut self) {
        self.scrub_resume = self.playing;
        self.playing = false;
        self.cut_drag = Some(CutDrag::Scrub);
    }

    pub fn mouse_up(&mut self) {
        self.source_copy_drag = None;
        if let Some(CutDrag::Band { x0, add, .. }) = self.cut_drag {
            if self.cut_band_rect().is_none() && !add {
                // A plain click on the vacant lanes: the playhead goes there.
                self.cut_drag = Some(CutDrag::Scrub);
                self.cut_drag_to(x0);
            }
            self.cut_band_base.clear();
        }
        if matches!(self.cut_drag, Some(CutDrag::Scrub | CutDrag::Band { .. })) && self.scrub_resume {
            self.playing = true;
        }
        self.scrub_resume = false;
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
        let end = (self.wrap - 0.05).max(0.0);
        let target = self.quantize(f * end).min(end);
        if (target - self.t).abs() < 1e-9 { return; }
        // Exact, always: this is a frame-precise tool. The player's frame
        // cache makes a seek inside the resident span instant, and the
        // reader keeps the GOP on screen and the next one warm.
        self.seek_all(target, true);
        self.adopt_seek = false;
    }

    pub fn scroll(&mut self, dx: f32, dy: f32) {
        self.stroke_last = None;
        if !self.ready() { return; }
        if self.cut_mode && self.show_ui && self.cut_tab == 1 && contains(self.cut_side().body, self.cursor.0, self.cursor.1) {
            self.info_scroll = (self.info_scroll.min(self.info_scroll_max()) - dy).clamp(0.0, self.info_scroll_max());
            return;
        }
        if self.cut_mode && self.show_ui && self.cut_over_panel() {
            let lane = self.cut_lanes().video;
            if dy.abs() > dx.abs() {
                // Two fingers up and down zoom about the pointer: swiping DOWN
                // dives in (the content under the pointer stays put), up backs out.
                let anchor = (self.cursor.0 - lane.x).clamp(0.0, lane.w);
                if let Some(cut) = &mut self.cut { cut.zoom((dy as f64 * 0.012).exp(), anchor, lane.w); }
            } else {
                // Sideways: fingers move the content along the track.
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
        if self.ready() && self.cut_mode && self.show_ui && self.cut_over_panel() {
            let lane = self.cut_lanes().video;
            let anchor = (self.cursor.0 - lane.x).clamp(0.0, lane.w);
            // Spreading zooms in, closing zooms out, anchored under the pointer.
            if let Some(cut) = &mut self.cut { cut.zoom(((delta as f64) * 1.8).exp(), anchor, lane.w); }
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
        let mut cell = self.video_cell(idx);
        if self.cut_mode && self.show_ui {
            // Timeline board: 764×430 footage centered in a 1140×583 canvas.
            let (dx, dy) = (cell.w * 188.0 / 1140.0, cell.h * 76.5 / 583.0);
            cell = RectPx { x: cell.x + dx, y: cell.y + dy, w: cell.w - 2.0*dx, h: cell.h - 2.0*dy };
            return Self::fit_rect(dims, cell);
        }
        Self::fit_rect(dims, inset_rect(cell, if self.show_ui { 24.0 } else { 0.0 }))
    }

    fn content_rect(&self, idx: usize) -> RectPx {
        Self::zoomed(self.base_rect(idx), self.zoom, self.center)
    }

    pub fn tick(&mut self, dt: f32, vp: (f32, f32), _scale: f32) -> FrameDesc {
        if self.vp != vp {
            self.stroke_last = None;
            self.vp = vp;
            self.reveal_source();
        }
        if let Some(cut) = &mut self.cut { cut.poll(); }
        if let Some(path) = self.cut.as_mut().and_then(|c| c.reload.take()) {
            let v = &mut self.videos[self.cut_video];
            if let Some(player) = Player::spawn(&path, v.info.width, v.info.height, crate::probe::vt_accel(&v.info.codec), v.info.rotation, self.cache_bytes) {
                v.player = player;
                v.info.path = path;
                v.linked_copy = false;
                v.delivered = false;
                self.seek_all(self.t, true);
                self.adopt_seek = false;
            } else if let Some(cut) = &mut self.cut { cut.status = "Encoded file saved, but its preview decoder could not open".into(); }
        }
        if self.frame_detail_boot && self.cut.as_ref().is_some_and(|c| !c.scanning()) {
            let width = self.cut_lanes().video.w;
            if let Some(c) = &mut self.cut {
                c.set_zoom_fraction(1.0, 0.0, width);
                log::debug!("frame detail: {} frames, {} px/s", c.frames.len(), c.pps);
            }
            self.frame_detail_boot = false;
        }
        self.tick_cut_thumbs(dt);
        self.update_split_preview();
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
        let superseded_seek = self.queued_seek.is_some();
        let mut adopt = None;
        for (i, v) in self.videos.iter_mut().enumerate() {
            let was_pending = v.pending;
            // A newer pointer target is waiting for this seek to settle.
            // Its intermediate frame must never reach the screen.
            let got = if superseded_seek {
                None
            } else if v.pending {
                let g = v.player.take_next();
                if g.is_some() {
                    v.pending = false;
                }
                g
            } else {
                v.player.take_upto(t + 1e-6)
            };
            if got.is_some() || v.player.failed() || v.player.buffered() > 0 {
                v.seeking = false;
            }
            if let Some((pts, buf)) = got {
                if was_pending && self.adopt_seek && i == active && !self.playing {
                    adopt = Some(pts);
                }
                v.shown_pts = pts;
                v.delivered = true;
                // The GPU's copy can't be read back, so a crop export needs
                // its own. Mask mode is paused: this runs on entry and on
                // seeks, not once a frame.
                if capture {
                    v.last_frame = Some(buf.clone());
                }
                if !self.split_preview.as_ref().is_some_and(|p| p.idx == i && p.shown) {
                    uploads.push(Upload { idx: i, w: v.player.w, h: v.player.h, buf });
                }
            }
        }
        if let Some(p) = &mut self.split_preview {
            if p.waiting && let Some((pts, buf)) = p.player.take_next() {
                p.waiting = false;
                p.shown = true;
                p.shown_pts = pts;
                uploads.push(Upload { idx: p.idx, w: p.player.w, h: p.player.h, buf });
            }
        }
        // Adopt once for a discrete seek/framestep. Pointer seeks retain
        // the requested clock even when decoding lands on a nearby frame.
        if !self.playing && let Some(pts) = adopt {
            self.t = pts;
        }
        // The seek that waited for this landing goes out now; the clock
        // already sits on its target.
        if let Some((target, exact)) = self.queued_seek && !self.seek_in_flight() {
            self.dispatch_seek(target, exact);
        }
        if !self.started {
            self.started = self.videos.iter().all(|v| v.delivered || v.player.failed());
        }
        self.badge_flash = (self.badge_flash - dt).max(0.0);
        let step = dt / RAIL_SLIDE_S;
        let toward = |cur: f32, shown: bool| {
            let target = if shown { 1.0 } else { 0.0 };
            if cur < target { (cur + step).min(target) } else { (cur - step).max(target) }
        };
        self.rail_slide = toward(self.rail_slide, self.show_sources);
        self.side_slide = toward(self.side_slide, self.show_inspector);
        let step = dt / DETAIL_SLIDE_S;
        let toward = |cur: f32, shown: bool| {
            let target = if shown { 1.0 } else { 0.0 };
            if cur < target { (cur + step).min(target) } else { (cur - step).max(target) }
        };
        self.detail_slide = toward(self.detail_slide, self.show_detail);
        self.tick_cut_hot(dt);

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
        if self.show_ui { items.push(Item::Rect(RectItem::new(canvas, if self.cut_mode { hex_color(CUT_CANVAS) } else { CANVAS_BG }))); }
        items.push(Item::Clip(Some(canvas)));
        // Keep the lifted picture attached to its zoomed bounds, while the
        // recessed document edge stays attached to the canvas itself.
        if self.show_ui {
            if mode == Mode::SideBySide {
                for i in 0..n {
                    items.push(Item::Shadow { r: self.content_rect(i), inner: false, blur: 18.0, alpha: 0.42 });
                }
            } else {
                items.push(Item::Shadow { r: self.content_rect(a), inner: false, blur: 18.0, alpha: 0.42 });
            }
        }
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
        if self.show_ui {
            items.push(Item::Shadow { r: canvas, inner: true, blur: 16.0, alpha: 0.28 });
        }
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
            self.playing || self.badge_flash > 0.0 || !self.started || self.cut_thumb.play.is_some() || self.cut_hot_animating()
            || self.rail_sliding();
        FrameDesc {
            clear: FRAME_BG,
            uploads,
            plate: None,
            thumbs: std::mem::take(&mut self.cut_thumb.uploads),
            items,
            animating,
            redraw_at: if animating {
                None
            } else {
                Some(Instant::now() + std::time::Duration::from_millis(
                    if self.split_preview.as_ref().is_some_and(|p| p.waiting) { 16 } else { 100 }))
            },
        }
    }

    /// Shared layout for drawing and hit testing; the image transform is always
    /// relative to this canvas, including in mask/crop and side-by-side modes.
    fn workspace(&self) -> Workspace {
        let mut w = Workspace::new(self.vp, self.show_ui, self.cut_mode, self.rail_slide, self.side_slide, self.detail_slide);
        if self.show_ui && self.cut_mode {
            let extra = self.frame_detail_extra() + if self.cut_encode_rail() { 26.0 } else { 0.0 };
            w.canvas.h = (w.canvas.h - extra).max(1.0);
            w.side.h = (w.side.h - extra).max(1.0);
            w.transport.y -= extra; w.timeline.y -= extra; w.timeline.h += extra;
        }
        w
    }

    fn cut_frame_cells(&self) -> bool {
        self.cut.as_ref().is_some_and(|c| c.pps / self.fps.max(1.0) >= 32.0)
    }

    /// Use the same visible-frame density as the marker renderer, including VFR.
    fn frame_markers_visible(&self) -> bool {
        let Some(c) = &self.cut else { return false };
        if c.frames.is_empty() { return false; }
        let width = self.cut_lanes().video.w;
        let lo = c.frames.partition_point(|t| *t < c.t0).saturating_sub(1);
        let hi = c.frames.partition_point(|t| *t <= c.t0 + c.span(width));
        hi - lo <= (width / 10.0) as usize + 1 && (lo..hi).any(|i|
            (c.frames.get(i+1).copied().unwrap_or(c.duration)-c.frames[i])*c.pps >= 10.0)
    }

    fn frame_detail(&self) -> bool {
        self.cut.as_ref().is_some_and(|c| !c.frames.is_empty() && c.pps * c.duration / c.frames.len().max(1) as f64 >= 80.0 - 1e-6)
    }

    fn frame_video_height(&self) -> f32 {
        if !self.frame_detail() { return 56.0; }
        let info = &self.videos[self.active].info;
        // Keep portrait pictures inside a bounded thumbnail, too.
        (154.0 * info.height as f32 / info.width.max(1) as f32).min(120.0) + 50.0
    }

    fn frame_detail_extra(&self) -> f32 {
        if self.frame_detail() { self.frame_video_height() - 56.0 + 36.0 + 44.0 } else { 0.0 }
    }

    pub fn start_frame_detail(&mut self, changes: bool) {
        self.set_cut_mode(true);
        self.frame_changes = changes;
        self.frame_detail_boot = true;
    }

    fn frame_mode_rects(&self) -> [RectPx; 2] {
        let tl = self.workspace().timeline;
        [0.0, 80.0].map(|dx| RectPx { x: tl.x + CUT_GUTTER + dx, y: tl.y + 4.0, w: 76.0, h: 28.0 })
    }

    /// Visible, kept frames in display order. These rects drive requests and paint.
    fn frame_tiles(&self) -> Vec<(usize, f64, char, bool, RectPx)> {
        if !self.frame_detail() { return Vec::new(); }
        let c = self.cut.as_ref().unwrap();
        let v = self.cut_lanes().video;
        let end = c.t0 + c.span(v.w);
        let lo = c.frames.partition_point(|t| *t < c.t0).saturating_sub(1);
        let hi = c.frames.partition_point(|t| *t < end);
        let segs = c.segments();
        let info=&self.videos[self.active].info;
        let aspect=info.width.max(1) as f32 / info.height.max(1) as f32;
        (lo..hi).take(256).filter_map(|i| {
            let t = c.frames[i];
            let seg = segs.iter().find(|s| !s.cut && t >= s.start && t < s.end)?;
            let to = c.frames.get(i+1).copied().unwrap_or(c.duration).min(seg.end);
            let fi = c.frame_info.partition_point(|f| f.pts < t - 1e-6);
            let info = c.frame_info.get(fi).filter(|f| (f.pts-t).abs() < 1e-6);
            let key = info.map_or_else(|| c.keys.binary_search_by(|k| k.total_cmp(&t)).is_ok(), |f| f.key);
            let width=(((to-t)*c.pps) as f32-4.0).clamp(7.0,160.0).min(120.0*aspect+6.0);
            Some((i, t, info.map_or('?', |f| f.kind), key, RectPx {
                x: v.x + ((t-c.t0)*c.pps) as f32 + 2.0, y: v.y + 24.0,
                w: width, h: (width-6.0)/aspect+26.0 }))
        }).collect()
    }

    fn frame_thumb_key(&self, index: usize, key: bool) -> i64 {
        -1 - index as i64 * 2 - i64::from(self.frame_changes && !key)
    }

    fn cut_encode_rail(&self) -> bool {
        self.cut_frame_cells() &&
            self.cut.as_ref().is_some_and(|c| !c.pending_gops().is_empty())
    }

    fn source_capacity(&self) -> usize {
        (self.workspace().list.h / SOURCE_H).floor().max(1.0) as usize
    }

    fn source_first(&self) -> usize {
        self.source_scroll.min(self.videos.len().saturating_sub(self.source_capacity()))
    }

    fn source_row(&self, index: usize) -> RectPx {
        let l = self.workspace();
        RectPx { x: l.list.x + 12.0, y: l.list.y + (index - self.source_first()) as f32 * SOURCE_H,
            w: l.panel - 24.0, h: SOURCE_H - 12.0 }
    }

    fn source_drop_index(&self, y: f32) -> usize {
        let first = self.source_first();
        (first..self.videos.len()).find(|&i| {
            let r = self.source_row(i);
            y < r.y + r.h / 2.0
        }).unwrap_or(self.videos.len())
    }

    /// The batch affordances (checkboxes, Clear) belong to Sources: mask
    /// paints one clip and the timeline cuts one.
    fn batch_ui(&self) -> bool {
        !self.mask_mode && !self.cut_mode
    }

    fn source_checkbox(&self, index: usize) -> RectPx {
        let r = self.source_row(index);
        RectPx { x: r.x + r.w - 39.0, y: r.y + 81.0, w: 20.0, h: 20.0 }
    }

    fn controls(&self) -> Vec<Control> {
        let l = self.workspace();
        let mut out = self.mode_selector().controls();
        out.push(Control::new(self.sidebar_toggle(), "", self.show_sources, Action::ToggleSources));
        if l.grip.h > 0.0 {
            out.push(Control::new(l.grip, "", self.show_detail, Action::ToggleDetail));
        }
        if self.cut_mode {
            if self.vp.0 >= 1000.0 {
                out.push(Control::new(self.inspector_toggle(), "", self.show_inspector, Action::ToggleInspector));
            }
            self.cut_controls(&mut out);
            return out;
        }
        if l.rail > 0.0 && !self.mask_mode {
            let w = (l.panel - 32.0) / 2.0;
            for (x, label, action) in [(l.list.x + 12.0, "Duplicate", Action::Duplicate),
                (l.list.x + 20.0 + w, "+ Add files", Action::OpenFiles)] {
                out.push(Control::new(RectPx { x, y: self.vp.1 - STATUS_H - 34.0, w, h: 28.0 }, label, false, action));
            }
            if self.videos.iter().any(|v| v.selected) {
                out.push(Control::new(RectPx { x: l.list.x + l.panel - 64.0, y: l.list.y + l.list.h + 4.0, w: 52.0, h: 24.0 },
                    "Clear", false, Action::ClearSources));
            }
        }
        let mut x = l.rail + 12.0;
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
            Action::Duplicate => self.duplicate_sources(),
            Action::OpenFiles => self.cmds.push(Cmd::OpenFiles),
            Action::ClearSources => { for v in &mut self.videos { v.selected = false; } },
            Action::ToggleSources => self.show_sources = !self.show_sources,
            Action::ToggleInspector => self.show_inspector = !self.show_inspector,
            Action::ToggleDetail => self.show_detail = !self.show_detail,
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

    /// Shared by the painted header and the native View menu.
    pub fn workspace_mode(&self) -> usize {
        if self.cut_mode { 3 } else if !self.mask_mode { 0 } else if self.crop.is_none() { 1 } else { 2 }
    }

    /// `--no-sources`: the rail hidden, as the header toggle leaves it.
    pub fn set_sources_visible(&mut self, shown: bool) {
        self.show_sources = shown;
        self.rail_slide = if shown { 1.0 } else { 0.0 };
    }

    /// `--no-inspector`: the timeline's inspector hidden, as its toggle leaves it.
    pub fn set_inspector_visible(&mut self, shown: bool) {
        self.show_inspector = shown;
        self.side_slide = if shown { 1.0 } else { 0.0 };
    }

    /// `--no-detail`: the rail's focused inspector folded, as its grip leaves it.
    pub fn set_detail_visible(&mut self, shown: bool) {
        self.show_detail = shown;
        self.detail_slide = if shown { 1.0 } else { 0.0 };
    }

    fn rail_sliding(&self) -> bool {
        self.rail_slide != if self.show_sources { 1.0 } else { 0.0 }
            || self.side_slide != if self.show_inspector { 1.0 } else { 0.0 }
            || self.detail_slide != if self.show_detail { 1.0 } else { 0.0 }
    }

    pub fn select_workspace_mode(&mut self, mode: usize) {
        if self.ready() && mode < 4 && mode != self.workspace_mode() {
            self.activate(Action::Tool(mode));
        }
    }

    /// Header (12:5): the sidebar toggle sits at x=78, clear of the traffic
    /// lights, and the mode selector 22px after it. Fullscreen has no
    /// lights, so both slide to the outer padding.
    fn sidebar_toggle(&self) -> RectPx {
        RectPx { x: if self.fullscreen { 14.0 } else { 78.0 }, y: 8.0, w: 32.0, h: 28.0 }
    }

    /// The right toggle (12:5 puts it 43px in from the edge); timeline only,
    /// and only when the window is wide enough to carry the inspector.
    fn inspector_toggle(&self) -> RectPx {
        RectPx { x: self.vp.0 - 43.0, y: 8.0, w: 32.0, h: 28.0 }
    }

    fn mode_selector(&self) -> ModeSelector {
        ModeSelector {
            rect: RectPx { x: self.sidebar_toggle().x + 54.0, y: 8.0, w: 385.0, h: 28.0 },
            active: self.workspace_mode(),
        }
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
        self.mode_selector().draw(items, self.cursor, self.cursor_inside);
        let toggle = self.sidebar_toggle();
        draw_sidebar_toggle(items, toggle, false, self.show_sources, self.cursor_inside && contains(toggle, self.cursor.0, self.cursor.1));
        for control in self.controls() {
            let hovered = contains(control.r, self.cursor.0, self.cursor.1) && self.cursor_inside;
            if matches!(control.action, Action::Tool(_) | Action::ToggleSources | Action::ToggleInspector | Action::ToggleDetail) {
                continue;
            }
            let disabled = matches!(control.action, Action::Save) && self.mask_save.is_some()
                || matches!(control.action, Action::Export) && self.crop_export.is_some();
            if matches!(control.action, Action::Duplicate | Action::OpenFiles) {
                items.push(Item::Rect(RectItem { radius: 8.0, border_w: 1.0,
                    border_color: hex_color(SOURCE_BORDER), ..RectItem::new(control.r, hex_color(SOURCE_SURFACE)) }));
            }
            if control.selected || hovered {
                items.push(Item::Rect(RectItem { radius: if control.selected { 7.0 } else { 6.0 },
                    ..RectItem::new(control.r, CONTROL_BG) }));
            }
            let color = if disabled { WORKSPACE_MUTED } else if control.selected || hovered { WORKSPACE_TEXT } else { WORKSPACE_DIM };
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

        self.build_sources(items);
        self.build_inspector(items);
        self.build_transport(items);
    }

    /// Focused inspector (278:609): on its own #0f0f0f ground under the
    /// rule, the clip's keycap and name as a title row, the five property
    /// rows between two dashed hairlines, and the directory in accent blue.
    fn build_inspector(&self, items: &mut Vec<Item>) {
        let l = self.workspace();
        if l.inspector.h > 0.0 {
            let v = &self.videos[self.active];
            let (y, x0, x1) = (l.inspector.y, l.inspector.x + 16.0, l.inspector.x + l.panel - 16.0);
            // The content keeps its full layout, anchored at the top, and
            // the visible rect clips it while the panel folds.
            items.push(Item::Clip(Some(RectPx { x: 0.0, y: l.inspector.y, w: l.rail, h: l.inspector.h })));
            items.push(Item::Rect(RectItem::new(RectPx { h: RAIL_INSPECTOR_H, ..l.inspector }, hex_color(INSPECTOR_BG))));
            let cap = RectPx { x: x0, y: y + 16.0, w: 24.0, h: 24.0 };
            items.push(Item::Rect(RectItem { radius: 12.0, ..RectItem::new(cap, mix(INSPECTOR_BG, 0xffffff, 0.5)) }));
            items.push(Item::Text(TextItem { header: true, align: Align::Center, valign: VAlign::Middle, max_width: Some(22.0),
                ..TextItem::new(cap.x + 12.0, cap.y + 12.0, 15.0, hex_color(0x000000), (self.active + 1).to_string()) }));
            items.push(Item::Text(TextItem { header: true, valign: VAlign::Middle, max_width: Some(l.panel - 80.0),
                ..TextItem::new(x0 + 32.0, y + 28.0, 13.0, hex_color(SOURCE_INK), name_of(&v.info.path)) }));
            // Dashed hairlines: rects, there is no line primitive.
            for ry in [y + 49.0, y + 178.0] {
                let mut x = x0;
                while x < x1 {
                    items.push(Item::Rect(RectItem::new(RectPx { x, y: ry, w: 3.0_f32.min(x1 - x), h: 1.0 }, hex_color(INSPECTOR_DASH))));
                    x += 6.0;
                }
            }
            for (i, (key, value)) in [
                ("Format", format!("{} {}", v.info.codec, v.info.pix_fmt)),
                ("Resolution", format!("{}×{}", v.info.width, v.info.height)),
                ("Framerate", format!("{:.3} fps", v.info.fps)),
                ("Bitrate", v.info.bit_rate.map(|b| format!("{:.2} Mb/s", b as f64 / 1e6)).unwrap_or_else(|| "—".into())),
                ("Size", fmt_size(v.info.file_size)),
            ].into_iter().enumerate() {
                let ry = y + 68.5 + i as f32 * 23.0;
                items.push(Item::Text(TextItem { regular: true, valign: VAlign::Middle, max_width: Some(78.0),
                    ..TextItem::new(x0, ry, 11.0, WORKSPACE_DIM, key) }));
                items.push(Item::Text(TextItem { regular: true, align: Align::Right, valign: VAlign::Middle, max_width: Some(l.panel - 110.0),
                    ..TextItem::new(x1, ry, 11.0, WORKSPACE_TEXT, value) }));
            }
            let dir = v.info.path.parent().unwrap_or_else(|| std::path::Path::new(""));
            let color = hex_color(INSPECTOR_PATH);
            // Folder outline, drawn as geometry so it works with the mono font.
            let folder = RectPx { x: x0, y: y + 189.0, w: 13.0, h: 10.0 };
            items.push(Item::Rect(RectItem { radius: 1.0, border_w: 1.3, border_color: color,
                ..RectItem::new(folder, [0.0; 4]) }));
            items.push(Item::Rect(RectItem { radius: 0.5, ..RectItem::new(
                RectPx { x: x0, y: y + 186.5, w: 5.0, h: 2.5 }, color) }));
            items.push(Item::Text(TextItem { regular: true, valign: VAlign::Middle, max_width: Some(l.panel - 55.0),
                ..TextItem::new(x0 + 19.0, y + 195.0, 10.0, color, dir.to_string_lossy()) }));
            items.push(Item::Clip(None));
        }
        if l.grip.h > 0.0 {
            // The grip, as the Figma draws it: an 8px #151515 strip along
            // the inspector's top edge under a 1px near-black line, with
            // three 5×4 discs (a #2a2a2a fill in a hairline #020202 outline) on
            // a 6px pitch centred in it. The strip sits at the bottom of the
            // 14px hit area; the rest is the rail's ground. Under the
            // pointer the discs lighten.
            let hover = self.cursor_inside && contains(l.grip, self.cursor.0, self.cursor.1);
            let strip = RectPx { x: l.grip.x, y: l.grip.y + l.grip.h - RAIL_GRIP_STRIP_H, w: l.panel, h: RAIL_GRIP_STRIP_H };
            items.push(Item::Rect(RectItem::new(RectPx { y: strip.y - 1.0, h: 1.0, ..strip }, hex_color(RAIL_GRIP_LINE))));
            items.push(Item::Rect(RectItem::new(strip, hex_color(RAIL_GRIP_BG))));
            let fill = hex_color(if hover { SOURCE_SUBTLE } else { RAIL_GRIP_DOT_FILL });
            let cx = l.grip.x + l.panel / 2.0;
            for k in -1..=1 {
                let (dw, dh) = RAIL_GRIP_DOT;
                let dot = RectPx { x: cx - dw / 2.0 + k as f32 * RAIL_GRIP_PITCH, y: strip.y + strip.h / 2.0 - dh / 2.0, w: dw, h: dh };
                items.push(Item::Rect(RectItem { radius: dh / 2.0, border_w: 0.75, border_color: hex_color(RAIL_GRIP_LINE),
                    ..RectItem::new(dot, fill) }));
            }
        }
    }

    fn inspector_path_hit(&self, x: f32, y: f32) -> bool {
        if !self.ready() || !self.show_ui || !self.cursor_inside { return false; }
        let l = self.workspace();
        let row = RectPx { x: l.inspector.x + 16.0, y: l.inspector.y + 183.0,
            w: (l.panel - 32.0).max(0.0), h: 24.0 };
        l.inspector.h > 0.0 && x < l.rail && contains(row, x, y) && y < l.inspector.y + l.inspector.h
            && self.videos[self.active].info.path.parent().is_some()
    }

    /// Sources sidebar (Abner — Timeline Edit, 278:584): a stack of
    /// preview cards — cover-fitted live media under a white keycap, then
    /// the filename and a two-column metadata line — ringed orange on the
    /// focused clip and on a navy surface otherwise. The hover checkbox,
    /// the shared/linked chip and the footer line are the app's batch
    /// affordances, kept where the design leaves room for them.
    fn build_sources(&self, items: &mut Vec<Item>) {
        let l = self.workspace();
        if l.rail <= 0.0 { return; }
        let ink = hex_color(SOURCE_INK);
        let secondary = hex_color(SOURCE_SECONDARY);
        let visible = RectPx { x: 0.0, y: l.list.y, w: l.rail, h: l.list.h };
        items.push(Item::Clip(Some(visible)));
        let first = self.source_first();
        for i in first..(first + self.source_capacity()).min(self.videos.len()) {
            let v = &self.videos[i];
            let r = self.source_row(i);
            let active = i == self.active;
            let hover = self.cursor_inside && contains(r, self.cursor.0, self.cursor.1);
            items.push(Item::Rect(RectItem { radius: 10.0, border_w: 2.0,
                border_color: hex_color(if active { SOURCE_RING } else if hover { SOURCE_EDGE_HOVER } else { SOURCE_EDGE }),
                ..RectItem::new(r, if active { WORKSPACE_PANEL } else { hex_color(0x000000) }) }));
            // The preview sits inside the ring AND the card's 2px padding —
            // the deliberate dark rim between ring and picture — with only
            // its top corners rounded: the quad runs 6px past the clip so
            // the bottom radius never shows.
            let thumb = RectPx { x: r.x + 4.0, y: r.y + 4.0, w: r.w - 8.0, h: SOURCE_THUMB_H };
            items.push(Item::Clip(Some(intersect(thumb, visible))));
            let quad = RectPx { h: thumb.h + 6.0, ..thumb };
            items.push(Item::Rect(RectItem { radius: 6.0, ..RectItem::new(quad, WORKSPACE_PANEL) }));
            if v.delivered {
                items.push(Item::Video { a: i, b: i, r: quad, uv: cover_uv(&v.info, quad),
                    mode: VideoMode::Tex, p0: 6.0, p1: 0.0 });
            }
            items.push(Item::Clip(Some(visible)));
            let badge = RectPx { x: thumb.x + 7.0, y: thumb.y + 7.0, w: 24.0, h: 24.0 };
            items.push(Item::Rect(RectItem { radius: 12.0, ..RectItem::new(badge, SOURCE_KEYCAP) }));
            items.push(Item::Text(TextItem { header: true, align: Align::Center, valign: VAlign::Middle, max_width: Some(22.0),
                ..TextItem::new(badge.x + 12.0, badge.y + 12.0, 15.0, hex_color(0x000000), (i + 1).to_string()) }));
            let shared = self.videos.iter().filter(|other| other.info.path == v.info.path).count();
            if shared > 1 || v.linked_copy {
                let chip = RectPx { x: thumb.x + 7.0, y: thumb.y + 70.0, w: (thumb.w - 48.0).min(142.0), h: 23.0 };
                items.push(Item::Rect(RectItem { radius: 5.0, ..RectItem::new(chip, hex_color(0x080d14)) }));
                ui_label(items, chip.x + 6.0, chip.y + 11.5, 10.0, secondary,
                    if shared > 1 { format!("Shared · {shared} items") } else { "Linked copy".into() }, Align::Left, chip.w - 12.0);
            }
            if self.batch_ui() && (hover || v.selected) {
                let check = self.source_checkbox(i);
                items.push(Item::Rect(RectItem { radius: 4.0, border_w: 1.0,
                    border_color: if v.selected { hex_color(CUT_HEADER_BLUE) } else { secondary },
                    ..RectItem::new(check, if v.selected { hex_color(CUT_HEADER_BLUE) } else { hex_color(0x080d14) }) }));
                if v.selected {
                    for (x, y) in [(0., 3.), (2., 5.), (4., 3.), (6., 1.), (8., -1.)] {
                        items.push(Item::Rect(RectItem::new(RectPx { x: check.x + 4.0 + x, y: check.y + 7.0 + y, w: 3.0, h: 3.0 }, ink)));
                    }
                }
            }
            // Text block: 7px side padding inside the ring, 6px below the
            // preview; the name and the metadata are 18px lines.
            let (tx, tw) = (thumb.x + 7.0, thumb.w - 14.0);
            let name = format!("{}{}", name_of(&v.info.path), if v.linked_copy { " · copy" } else { "" });
            items.push(Item::Text(TextItem { header: true, valign: VAlign::Middle, max_width: Some(tw),
                ..TextItem::new(tx, thumb.y + thumb.h + 15.0, 13.0, ink, name) }));
            let codec = match v.info.codec.as_str() { "h264" => "H.264".into(), "hevc" => "HEVC".into(), c => c.to_uppercase() };
            let seconds = v.info.duration.max(0.0) as u64;
            let meta_y = thumb.y + thumb.h + 33.0;
            let left = format!("{codec} · {}p", v.info.height);
            let right = format!("{}fps · {}:{:02}", v.info.fps, seconds / 60, seconds % 60);
            if l.panel >= 280.0 {
                items.push(Item::Text(TextItem { regular: true, valign: VAlign::Middle, max_width: Some(tw - 112.0),
                    ..TextItem::new(tx, meta_y, 11.0, secondary, left) }));
                items.push(Item::Text(TextItem { regular: true, align: Align::Right, valign: VAlign::Middle, max_width: Some(108.0),
                    ..TextItem::new(tx + tw, meta_y, 11.0, secondary, right) }));
            } else {
                items.push(Item::Text(TextItem { regular: true, valign: VAlign::Middle, max_width: Some(tw),
                    ..TextItem::new(tx, meta_y, 10.0, secondary, format!("{left} · {right}")) }));
            }
            if v.player.failed() {
                ui_label(items, thumb.x + 8.0, thumb.y + 52.0, 11.0, ERR, "DECODE FAILED", Align::Left, thumb.w - 16.0);
            }
        }
        items.push(Item::Clip(None));
        let n = self.videos.iter().filter(|v| v.selected).count();
        ui_label(items, l.list.x + 12.0, l.list.y + l.list.h + 16.0, 11.0, hex_color(SOURCE_SUBTLE),
            if n > 0 { format!("{n} checked · X toggles") }
            else if !self.source_status.is_empty() { self.source_status.clone() }
            else if self.videos.len() > self.source_capacity() { format!("{}–{} / {} · scroll", first+1, (first+self.source_capacity()).min(self.videos.len()), self.videos.len()) }
            else if self.source_file_count() < self.videos.len() { format!("{} files · 1–9 select", self.source_file_count()) }
            else { "1–9  Select a clip".into() }, Align::Left, l.panel - if n > 0 { 82.0 } else { 24.0 });
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
            ui_label(items, 16.0, y + STATUS_H / 2.0, 10.0, ACCENT, "TIMELINE", Align::Left, 76.0);
            let status = if !cut.status.is_empty() { cut.status.clone() } else if cut.scanning() { "Scanning keyframes…".into() } else {
                format!("{} · {} cut · snap to {} · {}", name_of(&cut.path), cut.cuts().len(),
                    if cut.snap == Snap::Keyframe { "keyframes" } else { "frames" },
                    if cut.copy_ready() { "ready to copy" } else { "encoding needed" })
            };
            let hint_width = if vp.0 >= 1000.0 { 440.0 } else { 0.0 };
            ui_label(items, 98.0, y + STATUS_H / 2.0, 10.0, WORKSPACE_DIM, status, Align::Left, vp.0 - 118.0 - hint_width);
            if hint_width > 0.0 {
                ui_label(items, vp.0 - 16.0, y + STATUS_H / 2.0, 10.0, WORKSPACE_MUTED,
                    "i in   o out   s split   x cut   u undo   k snap   e export   esc back", Align::Right, hint_width);
            }
            return;
        }
        let mode = if !self.mask_mode { "SOURCES" } else if self.crop.is_some() { "CROP" } else { "MASK" };
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
                items.push(Item::Thumb { r: inner, slot, radius: TILE_R - 1.0, alpha: 1.0, cover: true, clip_poster: false });
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
/// One immediate-mode component shared by Sources, Timeline, Crop and Mask.
/// Its controls also supply pointer and native-titlebar exclusion geometry.
struct ModeSelector {
    rect: RectPx,
    active: usize,
}

impl ModeSelector {
    /// Header / Container (122:441): 2px padding, 1px gaps, four equal segments.
    fn controls(&self) -> Vec<Control> {
        let w = (self.rect.w - 4.0 - 3.0 * 1.0) / 4.0;
        [("Sources", 0), ("Timeline", 3), ("Crop", 2), ("Mask", 1)]
            .into_iter().enumerate().map(|(index, (label, mode))| Control::new(
                RectPx { x: self.rect.x + 2.0 + index as f32 * (w + 1.0), y: self.rect.y + 2.0, w, h: self.rect.h - 4.0 },
                label, self.active == mode, Action::Tool(mode))).collect()
    }

    /// A #151515 housing with a 42%-ink hairline; the selected segment is
    /// lifted to #ededec with dark type, the rest sit at 34% ink. The
    /// design's alphas are baked with `mix()` over their ground — linear
    /// blending would read them far too bright.
    fn draw(&self, items: &mut Vec<Item>, cursor: (f32, f32), inside: bool) {
        let ground = 0x151515;
        items.push(Item::Rect(RectItem { radius: 4.0, border_w: 1.0, border_color: mix(ground, 0xededec, 0.42),
            ..RectItem::new(self.rect, hex_color(ground)) }));
        for control in self.controls() {
            let r = control.r;
            let hovered = inside && contains(r, cursor.0, cursor.1);
            if control.selected {
                items.push(Item::Rect(RectItem { radius: 3.0, ..RectItem::new(r, hex_color(0xededec)) }));
            }
            let color = if control.selected { hex_color(0x11151b) }
                else { mix(ground, 0xededec, if hovered { 0.7 } else { 0.34 }) };
            items.push(Item::Text(TextItem { header: true, align: Align::Center, valign: VAlign::Middle,
                tracking: 0.0337, max_width: Some(r.w - 8.0),
                ..TextItem::new(r.x + r.w / 2.0, r.y + r.h / 2.0, 11.0, color, control.label.to_uppercase()) }));
        }
    }
}

struct CutSide {
    tabs: RectPx,
    tab: [RectPx; 4],
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
        self.cut_restore = None;
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
            if self.cut_video != self.active || self.cut.as_ref().is_none_or(|c| c.path != info.path) {
                self.cut_video = self.active;
                self.cut = Some(Cut::open(&info.path, info.duration));
            }
            self.zoom = 1.0;
            self.center = (0.5, 0.5);
            self.cut_last_t = f64::NAN;
        }
        self.split_tool = false;
        self.cut_mode = on;
    }

    /// Cut mode's own keys. False = not one of them, fall through.
    fn cut_key(&mut self, k: Key) -> bool {
        let t = self.t;
        let action = match k {
            Key::Char('d' | 'D') => CutAction::FrameChanges(!self.frame_changes),
            Key::Char('`') => CutAction::TimeDisplay(!self.cut_frames),
            Key::Char('{') => CutAction::Chapter(-1),
            Key::Char('}') => CutAction::Chapter(1),
            Key::Char('i' | 'I') => CutAction::In,
            Key::Char('o' | 'O') => CutAction::Out,
            Key::Char('s' | 'S') => CutAction::Split,
            Key::Backspace if !self.cut_picks.is_empty() => CutAction::DeletePicked,
            Key::Char('x' | 'X') => CutAction::Apply,
            Key::Char('e' | 'E') => CutAction::Export,
            Key::Render => CutAction::EncodeAll,
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
                if self.split_tool { self.split_tool = false; return true; }
                if !self.cut_picks.is_empty() { self.cut_picks.clear(); return true; }
                if self.cut_restore.take().is_some() { return true; }
                let cut = self.cut.as_mut().unwrap();
                if cut.in_req.is_some() || cut.out_req.is_some() {
                    cut.clear_selection();
                } else {
                    self.set_cut_mode(false);
                }
                return true;
            }
            // One clip: nothing to flip to, no compare view to change.
            Key::Char('1'..='9' | 'v' | 'V' | '-' | '=' | '+') => return true,
            _ => return false,
        };
        self.cut_action(action);
        true
    }

    fn cut_action(&mut self, action: CutAction) {
        if matches!(action, CutAction::In | CutAction::Out | CutAction::Undo | CutAction::ToggleSplit) {
            self.cut_restore = None;
        }
        if action == CutAction::TogglePosters { self.cut_posters = !self.cut_posters; return; }
        if let CutAction::FrameChanges(changes) = action { self.frame_changes = changes; return; }
        if let CutAction::TimeDisplay(frames) = action { self.cut_frames = frames; return; }
        if action == CutAction::ToggleSplit {
            self.split_tool = !self.split_tool;
            self.cut_picks.clear();
            return;
        }
        let t = self.t;
        // S cuts where the razor shows, when it shows.
        let split_at = self.cut_razor().unwrap_or_else(|| self.split_target(t));
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
        if matches!(action, CutAction::Keyframe(_) | CutAction::Chapter(_)) {
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
            CutAction::Play | CutAction::TimeDisplay(_) | CutAction::FrameChanges(_) | CutAction::ToggleSplit | CutAction::TogglePosters => {}
            CutAction::Zoom(zoom_in) => {
                let px = ((t - cut.t0) * cut.pps) as f32;
                cut.zoom(if zoom_in { 1.5 } else { 1.0 / 1.5 }, if px >= 0.0 && px <= lane.w { px } else { lane.w / 2.0 }, lane.w);
            }
            CutAction::In => cut.set_in(t),
            CutAction::Out => cut.set_out(t),
            CutAction::Split => {
                if !cut.split(split_at) { cut.status = "No split here: already a boundary, or inside a cut".into(); }
            }
            CutAction::DeletePicked => {
                let picks = std::mem::take(&mut self.cut_picks);
                if !picks.is_empty() {
                    // The same range picked on both rows is one cut.
                    let mut ranges: Vec<(f64, f64)> = Vec::new();
                    for (a, b, _) in picks {
                        if self.cut_gop || cut.segments().iter().any(|s| !s.cut && (s.start - a).abs() < 1e-6 && (s.end - b).abs() < 1e-6) {
                            if !ranges.iter().any(|r| (r.0 - a).abs() < 1e-6 && (r.1 - b).abs() < 1e-6) { ranges.push((a, b)); }
                        }
                    }
                    if ranges.is_empty() { return; }
                    // A picked piece is already bounded by edits; don't expand it
                    // if the snap mode has changed since those boundaries were made.
                    // One undo step for the whole set.
                    let snap = cut.snap;
                    cut.snap = Snap::Frame;
                    let mut total = 0.0;
                    for (i, (a, b)) in ranges.iter().enumerate() {
                        cut.in_req = Some(*a);
                        cut.out_req = Some(*b);
                        cut.cut_selection();
                        if i > 0 { cut.merge_last_undo(); }
                        total += b - a;
                    }
                    cut.snap = snap;
                    cut.status = if ranges.len() == 1 { format!("Cut {total:.2} s") } else { format!("Cut {} clips, {total:.2} s", ranges.len()) };
                    return;
                }
            }
            CutAction::Apply => {
                if let Some((a, b)) = self.cut_restore.take() {
                    if cut.segments().iter().any(|s| s.cut && (s.start-a).abs() < 1e-6 && (s.end-b).abs() < 1e-6) {
                        let saved = (cut.in_req, cut.out_req, cut.snap);
                        cut.in_req = Some(a);
                        cut.out_req = Some(b);
                        cut.snap = Snap::Frame;
                        cut.cut_selection();
                        (cut.in_req, cut.out_req, cut.snap) = saved;
                        cut.status = format!("Restored {:.2} s", b-a);
                    }
                    return;
                }
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
            CutAction::EncodeAll => cut.start_encode(None),
            CutAction::Encode(t) => cut.start_encode(Some(t)),
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
        let top = if self.frame_detail() { 36.0 } else { 0.0 };
        let vh = self.frame_video_height();
        let extra = top + vh - 56.0 + if self.cut_encode_rail() { 26.0 } else { 0.0 };
        if self.vp.1 >= CUT_FULL_MIN_H {
            CutLanes { ruler: lane(2.0 + top, 14.0), chapters: Some(lane(20.0 + top, 24.0)), video: lane(48.0 + top, vh),
                audio: Some(lane(108.0 + extra, 39.0)), subs: lane(156.0 + extra, 17.0), keys: lane(180.0 + extra, 18.0) }
        } else {
            CutLanes { ruler: lane(2.0 + top, 14.0), chapters: None, video: lane(20.0 + top, vh),
                audio: None, subs: lane(84.0 + extra, 17.0), keys: lane(108.0 + extra, 18.0) }
        }
    }

    /// The header's Undo / Export and the scrub toolbar's icon buttons.
    /// `label` is what the button says when hovered; `hint` its key.
    fn cut_controls(&self, out: &mut Vec<Control>) {
        let Some(cut) = &self.cut else { return };
        let (w, bar) = (self.vp.0, self.workspace().transport);
        let mut push = |r: RectPx, label: &str, selected: bool, action: CutAction, hint: &'static str| {
            out.push(Control { hint, ..Control::new(r, label, selected, Action::Cut(action)) });
        };
        let export_x = if w >= 1000.0 { w - 154.0 } else { w - 115.0 };
        push(RectPx { x: export_x, y: 8.5, w: 101.0, h: 26.0 }, "Export", false, CutAction::Export, "");
        let video = self.cut_lanes().video;
        push(RectPx { x: self.workspace().timeline.x + 19.0, y: video.y + video.h / 2.0 - 12.0, w: 24.0, h: 24.0 },
            if self.cut_posters { "Hide clip previews" } else { "Show clip previews" }, self.cut_posters, CutAction::TogglePosters, "");
        if self.frame_detail() {
            for (r, changes, label) in [(self.frame_mode_rects()[0], false, "Decoded"), (self.frame_mode_rects()[1], true, "Changes")] {
                push(r, label, self.frame_changes == changes, CutAction::FrameChanges(changes), "D");
            }
        }
        let cy = bar.y + bar.h / 2.0;
        // The Figma transport has five controls. Chapters remain in the inspector.
        for (x, label, action, hint) in [
            (43.5, "Previous keyframe", CutAction::Keyframe(-1), "⇧←"),
            (74.5, "Back one frame", CutAction::Step(-1), "←"),
            (146.5, "Forward one frame", CutAction::Step(1), "→"),
            (177.5, "Next keyframe", CutAction::Keyframe(1), "⇧→"),
        ] {
            push(RectPx { x: bar.x + x, y: cy - 12.0, w: 24.0, h: 24.0 }, label, false, action, hint);
        }
        push(RectPx { x: bar.x + 105.5, y: cy - 17.0, w: 34.0, h: 34.0 }, if self.playing { "Pause" } else { "Play" }, false, CutAction::Play, "space");
        for (frames, offset, width, label) in [(false, 0.0, 30.0, "TIME"), (true, 30.0, 48.0, "FRAMES")] {
            push(RectPx { x: self.cut_clock_x() + offset, y: bar.y + 5.0, w: width, h: 16.0 },
                label, self.cut_frames == frames, CutAction::TimeDisplay(frames), "`");
        }
        let mut x = self.cut_tools_x();
        let keyframe = cut.snap == Snap::Keyframe;
        for (label, action, hint) in [
            ("Set in", CutAction::In, "I"), ("Set out", CutAction::Out, "O"),
            ("Split tool", CutAction::ToggleSplit, "Esc to leave · S split"),
            (match (self.cut_picks.len(), self.cut_gop) { (0, _) => if self.cut_restore.is_some() || cut.selection_is_cut() { "Restore selection" } else { "Cut selection" }, (1, true) => "Delete selected GOP", (1, false) => "Delete selected clip", (_, true) => "Delete selected GOPs", (_, false) => "Delete selected clips" }, if !self.cut_picks.is_empty() { CutAction::DeletePicked } else { CutAction::Apply }, if !self.cut_picks.is_empty() { "Backspace" } else { "X" }),
            (if keyframe { "Snap to keyframes: on" } else { "Snap to keyframes: off" },
                CutAction::Snap(if keyframe { Snap::Frame } else { Snap::Keyframe }), "K"),
        ] {
            push(RectPx { x, y: cy - 16.0, w: 32.0, h: 32.0 }, label, matches!(action, CutAction::Snap(_)) && keyframe, action, hint);
            x += 38.0;
        }
        let r = if w >= 1000.0 {
            RectPx { x: export_x - 118.0, y: 8.5, w: 108.0, h: 26.0 }
        } else if self.cut_clock_x() - x >= 120.0 {
            RectPx { x, y: cy - 16.0, w: 112.0, h: 32.0 }
        } else {
            // Keep Render reachable without covering the clock on narrow windows.
            RectPx { x: export_x - 118.0, y: 8.5, w: 108.0, h: 26.0 }
        };
        push(r, "Render", false, CutAction::EncodeAll, "⌘R");
        if bar.w >= 900.0 {
            let zoom = self.cut_zoom_bar();
            push(RectPx { x: zoom.x - 28.0, y: cy - 12.0, w: 24.0, h: 24.0 }, "Zoom out", false, CutAction::Zoom(false), "");
            push(RectPx { x: zoom.x + zoom.w + 4.0, y: cy - 12.0, w: 24.0, h: 24.0 }, "Zoom in", false, CutAction::Zoom(true), "");
        }
    }

    /// Edit tools follow playback; the clock follows the five edit buttons.
    fn cut_tools_x(&self) -> f32 {
        self.workspace().transport.x + 254.0
    }

    /// The clock is right-aligned beside zoom, or the right edge on narrow windows.
    fn cut_clock_x(&self) -> f32 {
        let bar = self.workspace().transport;
        if bar.w >= 900.0 { self.cut_zoom_bar().x - 200.0 }
        else { bar.x + bar.w - 180.0 }
    }

    /// The zoom slider's track, at the toolbar's right.
    fn cut_zoom_bar(&self) -> RectPx {
        let bar = self.workspace().transport;
        let width = if self.vp.0 < 900.0 { 60.0 } else { 110.0 };
        RectPx { x: bar.x + bar.w - 40.0 - width, y: bar.y + bar.h / 2.0 - 1.5, w: width, h: 3.0 }
    }

    fn encode_button(r: RectPx) -> RectPx {
        RectPx { x: r.x + (r.w - 48.0) / 2.0, y: r.y + (r.h - 22.0_f32.min(r.h)) / 2.0, w: 48.0, h: 22.0_f32.min(r.h) }
    }

    /// Shared paint/hit geometry, clipped to kept material and the viewport.
    fn cut_encode_regions(&self) -> Vec<(f64, RectPx)> {
        let Some(cut) = &self.cut else { return Vec::new() };
        let video = self.cut_lanes().video;
        let row = if self.cut_encode_rail() {
            RectPx { y: video.y + video.h + 3.0, h: 22.0, ..video }
        } else { video };
        let rail = self.cut_encode_rail();
        let mut out = Vec::new();
        for g in cut.pending_gops() {
            for s in cut.segments().iter().filter(|s| !s.cut) {
                let inset = if rail { 2.0 } else { 0.0 };
                let left = (row.x + ((g.encode_start.max(s.start) - cut.t0) * cut.pps) as f32 + inset).max(row.x);
                let right = (row.x + ((g.end.min(s.end) - cut.t0) * cut.pps) as f32 - inset).min(row.x + row.w);
                if right > left { out.push((g.start, RectPx { x: left, y: row.y + inset, w: right-left, h: row.h-2.0*inset })); }
            }
        }
        out
    }

    /// The source time under a pointer x on the lanes.
    fn cut_time_at(&self, x: f32) -> f64 {
        let lane = self.cut_lanes().video;
        self.cut.as_ref().map_or(0.0, |c| c.t0 + ((x - lane.x) as f64) / c.pps.max(1e-6))
    }

    /// Shared by the hover playhead and actual scrubbing, including frame cards.
    fn cut_scrub_target(&self, x: f32) -> f64 {
        let at = self.cut_time_at(x);
        let Some(cut) = &self.cut else { return at };
        if self.frame_detail() {
            let i = cut.frames.partition_point(|t| *t <= at).saturating_sub(1);
            cut.frames.get(i).copied().unwrap_or(at).min((self.wrap - 1e-6).max(0.0))
        } else {
            at.clamp(0.0, (cut.duration.min(self.wrap) - 0.05).max(0.0))
        }
    }

    fn cut_hover_playhead(&self) -> Option<f64> {
        if !self.cut_mode || !self.show_ui || !self.cursor_inside || self.split_tool || self.cut_drag.is_some() { return None; }
        let (x, y) = self.cursor;
        let v = self.cut_lanes().video;
        if !contains(self.workspace().timeline, x, y) || x < v.x || x >= v.x + v.w { return None; }
        // Only promise a seek where a click really scrubs, not trims, splits,
        // selects a removed range, or activates a timeline control.
        if self.cut_edge_at(x, y).is_some()
            || self.cut_ghost_at(x, y).is_some()
            || self.controls().iter().any(|c| contains(c.r, x, y))
            || self.cut_encode_regions().iter().any(|(_, r)| r.w >= 54.0 && contains(Self::encode_button(*r), x, y)) {
            return None;
        }
        Some(self.cut_scrub_target(x))
    }

    /// The clip edge under the pointer, if it is on one's grip: the video
    /// and audio rows, within a few px of a boundary that is on screen.
    fn cut_edge_at(&self, x: f32, y: f32) -> Option<f64> {
        let lanes = self.cut_lanes();
        let cut = self.cut.as_ref()?;
        let v = lanes.video;
        if self.frame_detail() && contains(v, x, y) { return None; }
        let bottom = lanes.audio.map_or(v.y + v.h, |a| a.y + a.h);
        if y < v.y || y >= bottom || x < v.x - CUT_GRIP || x > v.x + v.w + CUT_GRIP { return None; }
        let t = self.cut_time_at(x);
        let reach = CUT_GRIP as f64 / cut.pps.max(1e-6);
        cut.edges().into_iter().filter(|e| (e - t).abs() <= reach)
            .min_by(|a, b| (a - t).abs().total_cmp(&(b - t).abs()))
    }

    fn split_target(&self, t: f64) -> f64 {
        let Some(cut) = &self.cut else { return t };
        if cut.snap == Snap::Keyframe { cut.split_point(t) }
        else if !cut.frames.is_empty() { cut.next_frame(t) }
        else { ((t * self.fps.max(1.0) - 1e-7).ceil() / self.fps.max(1.0)).min(cut.duration) }
    }

    /// The split tool's preview and click share exactly the same target.
    fn cut_razor(&self) -> Option<f64> {
        if !self.cut_mode || !self.split_tool || !self.show_ui || !self.cursor_inside || self.cut_drag.is_some() { return None; }
        let (x, y) = self.cursor;
        let (segment, _) = self.cut_clip_at(x, y)?;
        let cut = self.cut.as_ref()?;
        if cut.snap == Snap::Keyframe && cut.keys.is_empty() { return None; }
        let at = self.split_target(self.cut_time_at(x));
        (at > segment.start + 1e-6 && at < segment.end - 1e-6).then_some(at)
    }

    fn restore_split_preview(&mut self, idx: usize, path: &std::path::Path) {
        if let Some(v) = self.videos.get_mut(idx).filter(|v| v.info.path == path) {
            v.player.seek(self.t, true);
            v.pending = true;
            self.adopt_seek = false;
        }
    }

    fn update_split_preview(&mut self) {
        let target = if self.hidden { None } else { self.cut_razor() };
        let same_source = self.split_preview.as_ref().is_some_and(|p|
            p.idx == self.active && self.videos.get(p.idx).is_some_and(|v| v.info.path == p.path));
        if (target.is_none() || !same_source) && let Some(p) = self.split_preview.take() {
            if p.shown { self.restore_split_preview(p.idx, &p.path); }
        }
        let Some(target) = target else { return };
        if self.split_preview.is_none() {
            let info = &self.videos[self.active].info;
            if let Some(player) = Player::spawn(&info.path, info.width, info.height,
                crate::probe::vt_accel(&info.codec), info.rotation, crate::player::SMALL_CACHE) {
                player.seek(target, true);
                self.split_preview = Some(SplitPreview { path: info.path.clone(), idx: self.active,
                    player, target, waiting: true, shown: false, shown_pts: 0.0 });
            }
        }
        let mut restore = None;
        if let Some(p) = &mut self.split_preview {
            if p.player.failed() {
                p.waiting = false;
                if p.shown { restore = Some((p.idx, p.path.clone())); p.shown = false; }
            } else if (p.target - target).abs() > 1e-9 {
                p.player.seek(target, true);
                p.target = target;
                p.waiting = true;
            }
        }
        if let Some((idx, path)) = restore { self.restore_split_preview(idx, &path); }
    }

    /// A press below the header in cut mode. True = it was the scrub
    /// panel's or the inspector's, whatever it hit there.
    fn cut_press(&mut self, x: f32, y: f32) -> bool {
        let l = self.workspace();
        if contains(l.transport, x, y) {
            let zoom = self.cut_zoom_bar();
            if l.transport.w >= 900.0 && contains(RectPx { x: zoom.x - 4.0, y: zoom.y - 10.0, w: zoom.w + 8.0, h: zoom.h + 20.0 }, x, y) {
                self.cut_drag = Some(CutDrag::Zoom);
                self.cut_drag_to(x);
            }
            return true;
        }
        if contains(l.timeline, x, y) {
            if let Some((at, r)) = self.cut_encode_regions().into_iter().find(|(_, r)| r.w >= 54.0 && contains(Self::encode_button(*r), x, y)) {
                let _ = r;
                self.cut_action(CutAction::Encode(at));
                return true;
            }
            if self.split_tool && self.cut_clip_at(x, y).is_some() {
                if let Some(at) = self.cut_razor() { self.cut.as_mut().unwrap().split(at); }
                return true;
            }
            if let Some(edge) = self.cut_edge_at(x, y) {
                if let Some(cut) = &mut self.cut { cut.begin_trim(); }
                self.playing = false;
                self.cut_drag = Some(CutDrag::Edge(edge));
            } else if let Some((seg, audio)) = self.cut_clip_at(x, y) {
                self.cut_restore = None;
                // A picture or sound clip: pick it (outlined), and still take the
                // playhead there. ⌘ toggles it in the set and leaves the playhead.
                let gop = self.frame_detail() && !audio;
                let range = if gop { self.cut.as_ref().unwrap().gop_at(self.cut_time_at(x)) } else { None };
                let gop = range.is_some();
                let (a, b) = range.unwrap_or((seg.start, seg.end));
                let pick = (a, b, audio);
                if self.press_cmd && gop == self.cut_gop {
                    if let Some(i) = self.cut_picks.iter().position(|p| Self::same_pick(*p, pick)) {
                        self.cut_picks.remove(i);
                    } else {
                        self.cut_picks.push(pick);
                    }
                } else {
                    self.cut_gop = gop;
                    self.cut_picks = vec![pick];
                    self.begin_cut_scrub();
                    self.cut_drag_to(x);
                }
            } else if let Some(seg) = self.cut_ghost_at(x, y) {
                // A removed clip is a selection: X restores it.
                self.cut_picks.clear();
                let cut = self.cut.as_mut().unwrap();
                self.cut_restore = Some((seg.start, seg.end));
                cut.status.clear();
            } else if self.cut_band_rows().is_some_and(|r| contains(r, x, y)) {
                // A vacant part of the lanes: a rubber band. ⌘ adds to the
                // picks; otherwise a plain click here lets go of them and
                // takes the playhead (`mouse_up`, when the band never grew).
                self.cut_restore = None;
                let add = self.press_cmd;
                if !add { self.cut_picks.clear(); }
                self.cut_band_base = self.cut_picks.clone();
                self.scrub_resume = self.playing;
                self.playing = false;
                self.cut_drag = Some(CutDrag::Band { x0: x, y0: y, add });
            } else if x >= self.cut_lanes().video.x - 4.0 {
                self.cut_restore = None;
                // Anywhere else on the timeline lets go of the pick.
                self.cut_picks.clear();
                self.begin_cut_scrub();
                self.cut_drag_to(x);
            }
            return true;
        }
        if contains(l.side, x, y) {
            let side = self.cut_side();
            if let Some(i) = side.tab.iter().position(|r| contains(*r, x, y)) {
                self.cut_tab = [0, 2, 3, 1][i];
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
            } else if let Some((_, start, _, _)) = self.cut_detail_rows().into_iter().find(|(r, ..)| contains(*r, x, y)) {
                let end = if self.wrap.is_finite() { (self.wrap - 0.05).max(0.0) } else { f64::MAX };
                self.seek_all(start.min(end), true);
            }
            return true;
        }
        false
    }

    /// The pointer is anywhere on the scrub panel — toolbar, ruler, gutter,
    /// lanes, the gaps between them — not just over a clip. Wheel and pinch
    /// zoom the track from all of it.
    fn cut_over_panel(&self) -> bool {
        let l = self.workspace();
        self.cursor_inside && (contains(l.timeline, self.cursor.0, self.cursor.1) || contains(l.transport, self.cursor.0, self.cursor.1))
    }

    /// The start-of-clip thumbnails: (key = start in ms, segment, rect). The
    /// poster fills the leading shaded area, underneath the trim grips, or
    /// sticks to the visible left edge while the start is scrolled away.
    fn build_frame_detail(&self, items: &mut Vec<Item>, column: RectPx) {
        let cut = self.cut.as_ref().unwrap();
        let v = self.cut_lanes().video;
        let x_of = |t: f64| v.x + ((t - cut.t0) * cut.pps) as f32;
        items.push(Item::Rect(RectItem::new(v, hex_color(0x0d0f10))));
        let selected: &[(f64, f64, bool)] = if self.cut_gop { &self.cut_picks } else { &[] };
        let lo = cut.keys.partition_point(|t| *t < cut.t0).saturating_sub(1);
        let hi = cut.keys.partition_point(|t| *t < cut.t0 + cut.span(v.w));
        for i in lo..hi {
            let a = cut.keys[i];
            let b = cut.keys.get(i+1).copied().unwrap_or(cut.duration);
            let left = x_of(a).max(v.x);
            let right = x_of(b).min(v.x+v.w);
            if right <= left { continue; }
            let on = selected.iter().any(|(s,e,_)| (s-a).abs()<1e-6 && (e-b).abs()<1e-6);
            let r = RectPx { x: left, y:v.y, w:right-left, h:v.h };
            items.push(Item::Rect(RectItem { radius:8.0, ..RectItem::new(r, hex_color(if on {0x202326} else {0x111313})) }));
            ui_label(items, left+8.0, v.y+11.0, 11.0, hex_color(if on {0xeeeeee} else {0x969da5}),
                format!("GOP {:02}{}", i+1, if on {"  ·  SELECTED"} else {""}), Align::Left, (r.w-16.0).max(0.0));
        }
        let info = &self.videos[self.active].info;
        let aspect = info.width.max(1) as f32 / info.height.max(1) as f32;
        for (index, _, kind, key, r) in self.frame_tiles() {
            let color = frame_color(kind,key);
            items.push(Item::Rect(RectItem { radius:8.0, border_w:3.0, border_color:color, ..RectItem::new(r,color) }));
            let ink = hex_color(0x1b1f22);
            let small = r.w < 120.0;
            let icon = if key { 0 } else { match kind { 'I'=>1, 'P'=>2, 'B'=>3, _=>0 } };
            // Match the Figma identity row: its icon box, label gap and inset.
            let (icon_x, icon_y, label_x) = match icon {
                0 => (3.0, 3.0, 25.0), 1 => (3.0, 1.5, 27.0),
                2 => (7.5, 2.0, 29.5), _ => (7.5, 3.0, 36.5),
            };
            if key || matches!(kind, 'I'|'P'|'B') {
                items.push(Item::FrameIcon { x:r.x+icon_x, y:r.y+icon_y, index:icon });
            }
            items.push(Item::Text(TextItem { header:true, valign:VAlign::Middle, tracking:1.4, max_width:Some((r.w-label_x-8.0).max(0.0)),
                ..TextItem::new(r.x+label_x,r.y+13.0,15.0,ink,if key {"KEY".into()} else {kind.to_string()}) }));
            if !small {
                items.push(Item::Text(TextItem { header:true, align:Align::Right,valign:VAlign::Middle,tracking:1.4,max_width:Some(r.w*0.45),
                    ..TextItem::new(r.x+r.w-7.0,r.y+13.0,15.0,ink,format!("{index:03}")) }));
            }
            let box_r = RectPx {x:r.x+3.0,y:r.y+23.0,w:(r.w-6.0).max(1.0),h:r.h-26.0};
            items.push(Item::Rect(RectItem {radius:7.0,..RectItem::new(box_r,hex_color(0x1b1f22))}));
            let w = box_r.w.min(box_r.h*aspect);
            let h = w/aspect;
            let image_r=RectPx{x:box_r.x+(box_r.w-w)/2.0,y:box_r.y+(box_r.h-h)/2.0,w,h};
            let tk = self.frame_thumb_key(index,key);
            if let Some(slot)=self.cut_thumb.slot_of(tk) {
                items.push(Item::Thumb {r:image_r,slot,radius:7.0,alpha:1.0,cover:false,clip_poster:false});
            } else {
                ui_label(items,box_r.x+box_r.w/2.0,box_r.y+box_r.h/2.0,10.0,hex_color(0x969da5),
                    if self.cut_thumb.failed.contains(&tk) {"Unavailable"} else {"…"},Align::Center,box_r.w);
            }
        }
        for (a,b,_) in selected.iter().copied() {
            let left=x_of(a).max(v.x);let right=x_of(b).min(v.x+v.w);
            if right>left {items.push(Item::Rect(RectItem{radius:8.0,border_w:2.0,border_color:hex_color(0xeeeeee),
                ..RectItem::new(RectPx{x:left,y:v.y,w:right-left,h:v.h},[0.0;4])}));}
        }
        if let Some(b)=self.cut_band_rect() {
            items.push(Item::Rect(RectItem::new(b,[ACCENT[0],ACCENT[1],ACCENT[2],0.18])));
            items.push(Item::Rect(RectItem{border_w:1.0,border_color:ACCENT,..RectItem::new(b,[0.0;4])}));
        }
        items.push(Item::Clip(Some(column)));
        let keys=self.cut_lanes().keys;
        let r=RectPx{x:v.x,y:keys.y+keys.h+3.0,w:v.w,h:36.0};
        items.push(Item::Rect(RectItem{radius:6.0,..RectItem::new(r,hex_color(0x1b1f22))}));
        let label=if selected.len()>1 {
            let total:f64=selected.iter().map(|(a,b,_)| b-a).sum();
            format!("{} GOPs selected  ·  {:.2} s  ·  Backspace deletes them", selected.len(), total)
        } else if let Some((a,b,_))=selected.last().copied() {
            let n=cut.keys.partition_point(|t| *t<=a);
            let lo=cut.frame_info.partition_point(|f| f.pts<a);
            let hi=cut.frame_info.partition_point(|f| f.pts<b);
            let frames=&cut.frame_info[lo..hi];
            format!("GOP {n:02}  ·  {} frames  ·  {} I / {} P / {} B  ·  {} – {}", frames.len(),
                frames.iter().filter(|f| f.kind=='I').count(),frames.iter().filter(|f| f.kind=='P').count(),frames.iter().filter(|f| f.kind=='B').count(),fmt_tc(a,self.fps),fmt_tc(b,self.fps))
        } else {"Select a frame to select its GOP · Scroll horizontally to pan".into()};
        ui_label(items,r.x+10.0,r.y+18.0,11.0,hex_color(0xeeeeee),label,Align::Left,r.w-20.0);
        let modes=self.frame_mode_rects();
        // Outside the time-column scissor, but still within the timeline panel.
        items.push(Item::Clip(Some(self.workspace().timeline)));
        ui_label(items,modes[1].x+modes[1].w+16.0,modes[0].y+14.0,11.0,hex_color(0x969da5),
            if self.frame_changes {"Changes vs previous decoded frame · white = unchanged"} else {"Complete decoded frames"},Align::Left,(v.w-300.0).max(0.0));
        if cut.zoom_fraction(v.w)>0.999 {ui_label(items,v.x+v.w-6.0,modes[0].y+14.0,10.0,hex_color(0x969da5),"MAX DETAIL",Align::Right,90.0);}
        items.push(Item::Clip(Some(column)));
    }

    /// The start-of-clip thumbnails: (key = start in ms, segment, rect).
    /// The poster fills the leading shaded area, underneath the trim grips, or
    /// sticks to the visible left edge while the start is scrolled away.
    fn cut_thumb_rects(&self) -> Vec<(i64, crate::cut::Segment, RectPx)> {
        let Some(cut) = self.cut.as_ref().filter(|_| self.cut_mode) else { return Vec::new() };
        if !self.cut_posters || self.frame_markers_visible() || self.frame_detail() { return Vec::new(); }
        let v = self.cut_lanes().video;
        let (t0, pps) = (cut.t0, cut.pps.max(1e-6));
        let x_of = |t: f64| v.x + ((t - t0) * pps) as f32;
        let h = v.h - 2.0;
        let w = (h * 16.0 / 9.0).round();
        cut.segments().into_iter().filter(|g| !g.cut).filter_map(|g| {
            let (l, r) = (x_of(g.start) + 1.0, x_of(g.end) - 1.0);
            let x = (l + 1.0).max(v.x);
            let w = w.min(r - 1.0 - x);
            if x >= v.x + v.w || w < 1.0 { return None; }
            Some(((g.start * 1000.0).round() as i64, g, RectPx { x, y: v.y + 1.0, w, h }))
        }).collect()
    }

    fn cut_thumb_hover(&self, rects: &[(i64, crate::cut::Segment, RectPx)]) -> Option<i64> {
        if !self.cursor_inside || !self.show_ui || !matches!(self.cut_drag, None | Some(CutDrag::Scrub)) { return None; }
        rects.iter().find(|(_, _, r)| contains(*r, self.cursor.0, self.cursor.1)).map(|(k, ..)| *k)
    }

    /// Fetch posters (clips on the picture lane, chapter cards), and play
    /// the hovered clip's.
    fn tick_cut_thumbs(&mut self, dt: f32) {
        if !self.cut_mode || self.cut.is_none() || self.videos.is_empty() {
            self.cut_thumb.play = None;
            self.cut_thumb.hover = None;
            return;
        }
        let info = self.videos[self.active].info.clone();
        if self.cut_thumb.path.as_ref() != Some(&info.path) {
            self.cut_thumb.reset();
            self.cut_thumb.path = Some(info.path.clone());
        }
        let rects = self.cut_thumb_rects();
        // Posters wanted on screen, as (key, at): the clips on the picture
        // lane, then the chapter cards while the inspector shows them.
        let mut wants: Vec<(i64, f64)> = rects.iter().map(|(k, g, _)| (*k, g.start)).collect();
        if self.cut_thumbs {
            let cut = self.cut.as_ref().unwrap();
            for (_, idx) in self.cut_chapter_rows() {
                let at = cut.chapters[idx].start;
                let key = (at * 1000.0).round() as i64;
                if !wants.iter().any(|w| w.0 == key) { wants.push((key, at)); }
            }
        }
        let tiles = self.frame_tiles();
        for &(index, at, _, key, _) in &tiles { wants.push((self.frame_thumb_key(index, key), at)); }
        let wanted = |key: i64| wants.iter().any(|w| w.0 == key);
        while let Ok((path, key, result)) = self.cut_thumb.rx.try_recv() {
            self.cut_thumb.pending.retain(|k| *k != key);
            if Some(&path) != self.cut_thumb.path.as_ref() { continue; }
            let rgba = match result {
                Ok(rgba) => rgba,
                Err(e) => { log::info!("cut thumbnail {key}ms: {e}"); self.cut_thumb.failed.push(key); continue; }
            };
            let slot = if self.cut_thumb.cells.len() < recent::ATLAS {
                self.cut_thumb.cells.push(ThumbCell { key, rgba: rgba.clone() });
                self.cut_thumb.cells.len() - 1
            } else if let Some(i) = self.cut_thumb.cells.iter().position(|c| !wanted(c.key)) {
                if self.cut_thumb.play.as_ref().is_some_and(|p| p.slot == i) { self.cut_thumb.play = None; }
                self.cut_thumb.cells[i] = ThumbCell { key, rgba: rgba.clone() };
                i
            } else { continue };
            self.cut_thumb.uploads.push(ThumbUpload { slot, buf: rgba });
        }
        for &(key, at) in &wants {
            let cb = &self.cut_thumb;
            if cb.pending.len() >= 2 { break; }
            let room = cb.cells.len() + cb.pending.len() < recent::ATLAS || cb.cells.iter().any(|c| !wanted(c.key));
            if cb.slot_of(key).is_some() || cb.pending.contains(&key) || cb.failed.contains(&key) || !room { continue; }
            self.cut_thumb.pending.push(key);
            let (tx, path) = (self.cut_thumb.tx.clone(), info.path.clone());
            let previous = if key < 0 && (-1-key) % 2 == 1 {
                let i = ((-1-key) / 2) as usize;
                i.checked_sub(1).and_then(|p| self.cut.as_ref().unwrap().frames.get(p).copied())
            } else { None };
            let spawn = std::thread::Builder::new().name("cut-thumb".into()).spawn(move || {
                let result = if key < 0 { recent::timeline_frame(&path, at, previous) } else { recent::frame(&path, at) }
                    .map_err(|e| format!("{e:#}"));
                let _ = tx.send((path, key, result));
            });
            if spawn.is_err() { self.cut_thumb.pending.retain(|k| *k != key); self.cut_thumb.failed.push(key); }
        }
        let hover = self.cut_thumb_hover(&rects);
        self.cut_thumb.hover = hover;
        let want = hover.filter(|k| self.cut_thumb.slot_of(*k).is_some());
        if self.cut_thumb.play.as_ref().map(|p| p.key) != want {
            if let Some(p) = self.cut_thumb.play.take() { self.cut_thumb.restore(p.slot); }
            if let (Some(key), true) = (want, !self.hidden) {
                let g = rects.iter().find(|r| r.0 == key).map(|r| r.1.clone());
                let slot = self.cut_thumb.slot_of(key);
                if let (Some(g), Some(slot)) = (g, slot) {
                    let h = ((recent::THUMB_W as f64 * info.height as f64 / info.width.max(1) as f64).round() as u32).max(2);
                    if let Some(player) = Player::spawn(&info.path, recent::THUMB_W, h, crate::probe::vt_accel(&info.codec), info.rotation, crate::player::SMALL_CACHE) {
                        player.seek(g.start, true);
                        self.cut_thumb.play = Some(ThumbPlay { key, slot, player, start: g.start, end: g.end, fps: info.fps.max(1.0), t: g.start });
                    }
                }
            }
        }
        let mut stopped = None;
        if let Some(p) = &mut self.cut_thumb.play {
            if p.player.failed() {
                stopped = Some(p.slot);
            } else {
                p.t += dt as f64;
                if p.t >= p.end - 0.5 / p.fps {
                    p.player.seek(p.start, true);
                    p.t = p.start;
                }
                if let Some((_, buf)) = p.player.take_upto(p.t + 1e-6) {
                    let cell = fit_cell(&buf, p.player.w, p.player.h);
                    self.cut_thumb.uploads.push(ThumbUpload { slot: p.slot, buf: cell });
                }
            }
        }
        if let Some(slot) = stopped {
            self.cut_thumb.play = None;
            self.cut_thumb.restore(slot);
        }
    }

    /// The kept picture clip under the pointer.
    /// Ease each clip's hover lift toward its target: a quick rise under the
    /// pointer, a slower fall after it leaves. Settled zeros are dropped.
    fn tick_cut_hot(&mut self, dt: f32) {
        const RISE: f32 = 0.06;
        const FALL: f32 = 0.14;
        let target = if self.cut_mode && self.show_ui && self.cursor_inside && matches!(self.cut_drag, None | Some(CutDrag::Scrub)) {
            self.cut_clip_at(self.cursor.0, self.cursor.1).map(|(g, snd)| ((g.start * 1000.0).round() as i64, snd))
        } else {
            None
        };
        if let Some((key, snd)) = target && !self.cut_hot.iter().any(|h| h.0 == key && h.1 == snd) {
            self.cut_hot.push((key, snd, 0.0));
        }
        for h in &mut self.cut_hot {
            let on = target == Some((h.0, h.1));
            h.2 = if on { (h.2 + dt / RISE).min(1.0) } else { (h.2 - dt / FALL).max(0.0) };
        }
        self.cut_hot.retain(|h| h.2 > 0.0 || target == Some((h.0, h.1)));
    }

    fn cut_hot_animating(&self) -> bool {
        let target = if self.cut_mode && self.show_ui && self.cursor_inside && matches!(self.cut_drag, None | Some(CutDrag::Scrub)) {
            self.cut_clip_at(self.cursor.0, self.cursor.1).map(|(g, snd)| ((g.start * 1000.0).round() as i64, snd))
        } else {
            None
        };
        self.cut_hot.iter().any(|h| if target == Some((h.0, h.1)) { h.2 < 1.0 } else { h.2 > 0.0 })
    }

    /// The eased hover lift (0..1) of a kept clip.
    fn cut_hot_level(&self, start: f64, snd: bool) -> f32 {
        let key = (start * 1000.0).round() as i64;
        self.cut_hot.iter().find(|h| h.0 == key && h.1 == snd).map_or(0.0, |h| h.2)
    }

    fn cut_clip_at(&self, x: f32, y: f32) -> Option<(crate::cut::Segment, bool)> {
        let lanes = self.cut_lanes();
        let cut = self.cut.as_ref()?;
        // No sound lane is drawn once the audio read found nothing.
        let sound = lanes.audio.filter(|_| !(cut.wave.is_empty() && !cut.reading_wave()));
        let audio = match (y, sound) {
            (y, _) if y >= lanes.video.y && y < lanes.video.y + lanes.video.h => false,
            (y, Some(a)) if y >= a.y && y < a.y + a.h => true,
            _ => return None,
        };
        if x < lanes.video.x || x >= lanes.video.x + lanes.video.w { return None; }
        let at = self.cut_time_at(x);
        cut.segments().into_iter().find(|g| !g.cut && at >= g.start && at < g.end).map(|g| (g, audio))
    }

    /// The removed clip under the pointer on the picture or sound row.
    fn cut_ghost_at(&self, x: f32, y: f32) -> Option<crate::cut::Segment> {
        let lanes = self.cut_lanes();
        let rows = [Some(lanes.video), lanes.audio].into_iter().flatten();
        if !rows.into_iter().any(|r| y >= r.y && y < r.y + r.h) { return None; }
        let at = self.cut_time_at(x);
        self.cut.as_ref()?.segments().into_iter().find(|g| g.cut && at >= g.start && at < g.end)
    }

    fn same_pick(a: (f64, f64, bool), b: (f64, f64, bool)) -> bool {
        a.2 == b.2 && (a.0 - b.0).abs() < 1e-3 && (a.1 - b.1).abs() < 1e-3
    }

    /// The block a rubber band can start on and sweep: the picture row down
    /// to the foot of the keyframe row, across the lanes' width — a fitted
    /// clip fills its own row, so the sound, caption and keyframe rows are
    /// where a band usually begins. The ruler above stays a scrub. None
    /// without a timeline, or before the first draw fitted it (no scale =
    /// no time under the pointer).
    fn cut_band_rows(&self) -> Option<RectPx> {
        let lanes = self.cut_lanes();
        self.cut.as_ref().filter(|c| c.pps > 0.0)?;
        let bottom = lanes.keys.y + lanes.keys.h;
        Some(RectPx { x: lanes.video.x, y: lanes.video.y, w: lanes.video.w, h: bottom - lanes.video.y })
    }

    /// The band's rectangle while one is being dragged, clamped to the rows
    /// it sweeps; None once it is still too small to count as a drag.
    fn cut_band_rect(&self) -> Option<RectPx> {
        let Some(CutDrag::Band { x0, y0, .. }) = self.cut_drag else { return None };
        let rows = self.cut_band_rows()?;
        let (x1, y1) = self.cursor;
        if (x1 - x0).abs() < BAND_MIN && (y1 - y0).abs() < BAND_MIN { return None; }
        let (l, r) = (x0.min(x1).max(rows.x), x0.max(x1).min(rows.x + rows.w));
        let (t, b) = (y0.min(y1).max(rows.y), y0.max(y1).min(rows.y + rows.h));
        Some(RectPx { x: l, y: t, w: (r - l).max(0.0), h: (b - t).max(0.0) })
    }

    /// Re-pick from the band: every kept clip (or GOP, in frame detail)
    /// whose rect the band touches, on each row it crosses.
    fn cut_band_pick(&mut self) {
        let Some(CutDrag::Band { add, .. }) = self.cut_drag else { return };
        let Some(band) = self.cut_band_rect() else {
            self.cut_picks = self.cut_band_base.clone();
            return;
        };
        let lanes = self.cut_lanes();
        let Some(cut) = self.cut.as_ref() else { return };
        let sound = lanes.audio.filter(|_| !(cut.wave.is_empty() && !cut.reading_wave()));
        let (t0, t1) = (self.cut_time_at(band.x), self.cut_time_at(band.x + band.w));
        let crosses = |row: RectPx| band.y < row.y + row.h && band.y + band.h > row.y;
        let gop = self.frame_detail();
        let segs = cut.segments();
        let mut hits: Vec<(f64, f64, bool)> = Vec::new();
        for (row, audio) in [(Some(lanes.video), false), (sound, true)] {
            let Some(row) = row else { continue };
            if !crosses(row) { continue; }
            if gop && !audio {
                let lo = cut.keys.partition_point(|k| *k <= t0).saturating_sub(1);
                for i in lo..cut.keys.len() {
                    let a = cut.keys[i];
                    let b = cut.keys.get(i + 1).copied().unwrap_or(cut.duration);
                    if a >= t1 { break; }
                    if b > t0 && a < t1 && !segs.iter().any(|s| s.cut && s.start <= a + 1e-6 && s.end >= b - 1e-6) {
                        hits.push((a, b, false));
                    }
                }
            } else {
                for g in segs.iter().filter(|g| !g.cut && g.end > t0 && g.start < t1) {
                    hits.push((g.start, g.end, audio));
                }
            }
        }
        let mut picks = if add && gop == self.cut_gop { self.cut_band_base.clone() } else { Vec::new() };
        for h in hits {
            if !picks.iter().any(|p| Self::same_pick(*p, h)) { picks.push(h); }
        }
        self.cut_gop = gop;
        self.cut_picks = picks;
    }

    fn cut_drag_to(&mut self, x: f32) {
        let lane = self.cut_lanes().video;
        let zoom = self.cut_zoom_bar();
        let at = self.cut_time_at(x);
        let scrub_target = self.cut_scrub_target(x);
        let playhead = self.t;
        let Some(cut) = self.cut.as_mut() else { return };
        let end = (cut.duration.min(self.wrap) - 0.05).max(0.0);
        match self.cut_drag {
            Some(CutDrag::Scrub) => {
                let target = scrub_target;
                if (target - self.t).abs() > 1e-9 { self.seek_all(target, true); }
                self.adopt_seek = false;
            },
            Some(CutDrag::Edge(edge)) => {
                // The canvas follows the edge, so the trim is made by eye.
                let now = cut.move_edge(edge, at);
                self.cut_drag = Some(CutDrag::Edge(now));
                self.seek_all(now.min(end), true);
                self.adopt_seek = false;
            }
            Some(CutDrag::Zoom) => {
                // About the playhead when it is on screen, the middle if not.
                let px = ((playhead - cut.t0) * cut.pps) as f32;
                let anchor = if px >= 0.0 && px <= lane.w { px } else { lane.w / 2.0 };
                cut.set_zoom_fraction(((x - zoom.x) / zoom.w).clamp(0.0, 1.0) as f64, anchor, lane.w);
            }
            Some(CutDrag::Band { .. }) => self.cut_band_pick(),
            None => {}
        }
    }

    /// The inspector's geometry, shared by drawing and hit testing: icon
    /// tabs, the chip strip, the count row with the list / thumbnails
    /// toggle, the body, and a footer holding the result.
    fn cut_side(&self) -> CutSide {
        let s = self.workspace().side;
        let (x0, w) = (s.x + 16.0, s.w - 32.0);
        // No clip header here: the Sources rail already shows the clip in
        // timeline mode, so the icon tabs start at the top.
        let tabs = RectPx { x: x0, y: s.y + 16.0, w, h: 32.0 };
        let half = (w - 4.0) / 4.0;
        let tab = [0, 1, 2, 3].map(|i| RectPx { x: x0 + 2.0 + i as f32 * half, y: tabs.y + 2.0, w: half, h: 28.0 });
        let count_y = tabs.y + tabs.h + 14.0;
        let toggle = RectPx { x: x0 + w - 58.0, y: count_y, w: 58.0, h: 26.0 };
        let add_btn = RectPx { x: toggle.x - 34.0, y: toggle.y + 2.0, w: 26.0, h: 22.0 };
        let list_btn = RectPx { x: toggle.x + 2.0, y: toggle.y + 2.0, w: 26.0, h: 22.0 };
        let grid_btn = RectPx { x: toggle.x + 30.0, ..list_btn };
        let footer_y = s.y + s.h - 56.0;
        let warning = self.cut.as_ref().and_then(|c| c.warning()).map(|w| wrap_text(&w, 38)).unwrap_or_default();
        let warning_h = if warning.is_empty() { 0.0 } else { 16.0 + warning.len() as f32 * 15.0 };
        let warning_y = footer_y - 10.0 - warning_h;
        let body_top = if self.cut_tab == 0 { count_y + 24.0 + 10.0 } else { tabs.y + tabs.h + 16.0 };
        let body_bottom = if warning.is_empty() { footer_y - 10.0 } else { warning_y - 8.0 };
        CutSide { tabs, tab, count_y, toggle, add_btn, list_btn, grid_btn, footer_y, warning, warning_y, warning_h,
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

    /// Subtitle and segment lists share their drawing and seek targets.
    fn cut_detail_rows(&self) -> Vec<(RectPx, f64, f64, String)> {
        let Some(cut) = &self.cut else { return Vec::new() };
        let entries: Vec<_> = match self.cut_tab {
            2 => cut.cues.iter().map(|c| (c.start, c.end, c.text.replace('\n', " "))).collect(),
            3 => cut.segments().iter().map(|g| (g.start, g.end, format!("{} {}", if g.cut { "Cut" } else { "Keep" }, g.number))).collect(),
            _ => return Vec::new(),
        };
        let body = self.cut_side().body;
        let fit = ((body.h - 28.0).max(0.0) / 52.0).floor() as usize;
        let current = entries.iter().rposition(|(start, ..)| *start <= self.t).unwrap_or(0);
        let first = current.saturating_sub(fit / 2).min(entries.len().saturating_sub(fit));
        entries.into_iter().skip(first).take(fit).enumerate().map(|(i, (start, end, title))| {
            (RectPx { x: body.x, y: body.y + 28.0 + i as f32 * 52.0, w: body.w, h: 46.0 }, start, end, title)
        }).collect()
    }

    /// What the frame under the playhead is, on the footage: keyframe or
    /// not, its number, and the keyframes either side.
    fn build_cut_chips(&self, items: &mut Vec<Item>) {
        let Some(cut) = &self.cut else { return };
        let shown_time = self.split_preview.as_ref().filter(|p| p.shown).map_or(self.t, |p| p.shown_pts);
        let canvas = self.workspace().canvas;
        let image = self.content_rect(self.active);
        let x = (image.x - 9.0).max(canvas.x + 8.0);
        let y = image.y.max(canvas.y) + 44.0;
        if image.h < 100.0 { return; }
        if !cut.keys.is_empty() {
            let on = cut.on_key(shown_time, 1.0 / self.fps);
            items.push(Item::Text(TextItem { valign: VAlign::Middle,
                bg: Some(TextBg { radius: 4.0, pad_x: 11.0, pad_y: 3.0,
                    ..TextBg::new(if on { hex_color(FRAME_KEY) } else { hex_color(CUT_BG) }) }),
                ..TextItem::new(x + 11.0, y, 10.5, if on { hex_color(CUT_AMBER_INK) } else { hex_color(CUT_INK) },
                    if on { "I · KEY" } else { "between keys" }) }));
        }
        if let Some((i, chapter)) = cut.chapters.iter().enumerate().rfind(|(_, c)| c.start <= shown_time + 1e-6) {
            items.push(Item::Text(TextItem { valign: VAlign::Middle, max_width: Some((image.w - 30.0).max(0.0)),
                bg: Some(TextBg { radius: 3.0, pad_x: 11.0, pad_y: 3.0, ..TextBg::new(hex_color(CUT_CHAPTER_TAG)) }),
                ..TextItem::new(x + 11.0, y + 30.0, 10.5, hex_color(0xffffff), format!("{:02} {}", i+1, chapter.title)) }));
        }
    }

    /// Figma x=78 (the sidebar toggle) clears the traffic lights; fullscreen needs only outer padding.
    /// Only unoccupied header background behaves like a native titlebar.
    /// Keep the entire tab housing (including gaps) and action buttons interactive.
    pub fn titlebar_background(&self, x: f32, y: f32) -> bool {
        if self.fullscreen || !self.show_ui || !(78.0..self.vp.0).contains(&x)
            || !(0.0..HEADER_H).contains(&y) { return false; }
        !self.ready() || (!contains(self.header_tabs(), x, y)
            && !self.controls().iter().any(|c| contains(c.r, x, y)))
    }

    pub fn native_header_controls(&self) -> Vec<RectPx> {
        if !self.show_ui || !self.ready() { return Vec::new(); }
        let mut rects = vec![self.header_tabs()];
        rects.extend(self.controls().into_iter().filter(|c| c.r.y < HEADER_H).map(|c| c.r));
        rects
    }

    fn header_tabs(&self) -> RectPx { self.mode_selector().rect }

    fn build_cut_header(&self, items: &mut Vec<Item>) {
        let r = self.header_tabs();
        self.mode_selector().draw(items, self.cursor, self.cursor_inside);
        let toggle = self.sidebar_toggle();
        draw_sidebar_toggle(items, toggle, false, self.show_sources, self.cursor_inside && contains(toggle, self.cursor.0, self.cursor.1));
        if self.vp.0 >= 1000.0 {
            let toggle = self.inspector_toggle();
            draw_sidebar_toggle(items, toggle, true, self.show_inspector, self.cursor_inside && contains(toggle, self.cursor.0, self.cursor.1));
            let cut = self.cut.as_ref().unwrap();
            let (text, color) = if cut.encoding() {
                ("ENCODING SPLIT GOPs…", hex_color(0xb68aff))
            } else if cut.scanning() { ("SCANNING KEYFRAMES…", hex_color(CUT_INK))
            } else if cut.copy_ready() {
                ("READY · STREAM COPY", hex_color(CUT_LAMP))
            } else { ("SPLIT GOPs · ENCODING NEEDED", hex_color(0xb68aff)) };
            let status_right = self.vp.0 - 285.0;
            items.push(Item::Text(TextItem { align: Align::Right, valign: VAlign::Middle,
                tracking: 0.63, max_width: Some((status_right - 12.0 - r.x - r.w).max(0.0)),
                ..TextItem::new(status_right, 21.5, 10.5, color, text) }));
        }
    }

    fn build_cut_hud(&self, items: &mut Vec<Item>, vp: (f32, f32)) {
        let l = self.workspace();
        let cut = self.cut.as_ref().unwrap();
        let rule = mix(0x050506, 0xffffff, 0.06);
        // Opaque shell surfaces, as in Input.
        items.push(Item::Rect(RectItem::new(RectPx { x: 0.0, y: 0.0, w: vp.0, h: CUT_HEADER_H }, WORKSPACE_PANEL)));
        items.push(Item::Rect(RectItem::new(l.transport, hex_color(CUT_BAR))));
        items.push(Item::Rect(RectItem::new(l.timeline, hex_color(CUT_TL))));
        items.push(Item::Rect(RectItem::new(l.side, hex_color(CUT_BG))));
        items.push(Item::Rect(RectItem::new(RectPx { x: 0.0, y: CUT_HEADER_H, w: l.rail, h: l.transport.y - CUT_HEADER_H }, WORKSPACE_PANEL)));
        for r in [RectPx { x: 0.0, y: CUT_HEADER_H - 1.0, w: vp.0, h: 1.0 },
            RectPx { h: 1.0, ..l.transport }, RectPx { h: 1.0, ..l.timeline },
            RectPx { w: if l.side.w > 0.0 { 1.0 } else { 0.0 }, ..l.side },
            RectPx { x: l.rail - 1.0, y: CUT_HEADER_H, w: if l.rail > 0.0 { 1.0 } else { 0.0 }, h: l.transport.y - CUT_HEADER_H }] {
            items.push(Item::Rect(RectItem::new(r, rule)));
        }
        self.build_sources(items);
        self.build_inspector(items);

        // ---- Figma timeline header ----
        let controls = self.controls();
        self.build_cut_header(items);

        // ---- scrub toolbar: play, the clock, the edit tools, the zoom ----
        let bar = l.transport;
        let cy = bar.y + bar.h / 2.0;
        let x = self.cut_clock_x();
        items.push(Item::Rect(RectItem { radius: 6.0, ..RectItem::new(
            RectPx { x: x - 9.0, y: bar.y + 4.0, w: 148.0, h: 35.0 }, hex_color(0x020202)) }));
        let clock_y = bar.y + 30.5;
        let clock_px = 20.0;
        if self.cut_frames {
            ui_label(items, x, clock_y, clock_px, hex_color(CUT_INK),
                format!("{}", (self.t * self.fps).round() as u64), Align::Left, 123.0);
        } else {
            let tc = fmt_tc(self.t, self.fps);
            let (clock, frames) = tc.split_at(8);
            items.push(Item::Text(TextItem { valign: VAlign::Middle, tracking: -0.2,
                ..TextItem::new(x, clock_y, clock_px, hex_color(CUT_INK), clock) }));
            // Same mono run origin: let the renderer measure the eight advances.
            items.push(Item::Text(TextItem { valign: VAlign::Middle, tracking: -0.2,
                ..TextItem::new(x, clock_y, clock_px, mix(CUT_BAR, 0xffffff, 0.3), format!("        {frames}")) }));
        }
        for (offset, alpha) in [(13.0, 0.2), (231.0, 0.25), (x - bar.x - 12.0, 0.25)] {
            items.push(Item::Rect(RectItem::new(RectPx { x: bar.x + offset, y: cy - 10.0, w: 1.0, h: 20.0 }, mix(CUT_BAR, 0xffffff, alpha))));
        }
        if bar.w >= 900.0 {
            let zoom = self.cut_zoom_bar();
            items.push(Item::Rect(RectItem { radius: 1.5, ..RectItem::new(zoom, mix(CUT_BAR, 0xffffff, 0.14)) }));
            let knob = zoom.x + zoom.w * cut.zoom_fraction(self.cut_lanes().video.w) as f32;
            items.push(Item::Rect(RectItem { radius: 5.5, ..RectItem::new(RectPx { x: knob - 5.5, y: cy - 5.5, w: 11.0, h: 11.0 }, hex_color(CUT_INK)) }));
            items.push(Item::Rect(RectItem::new(RectPx { x: zoom.x - 39.0, y: cy - 10.0, w: 1.0, h: 20.0 }, mix(CUT_BAR, 0xffffff, 0.1))));
        }

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
            return;
        };
        let soft = mix(0x050506, 0xffffff, 0.72);
        match action {
            CutAction::TogglePosters => {
                draw_icon(items, Icon::TrackFilm, cx, cy, hex_color(
                    if hovered { 0xd1d1d1 } else if self.cut_posters { 0x626262 } else { 0x383838 }));
            }
            CutAction::FrameChanges(changes) => {
                items.push(Item::Rect(RectItem { radius: 5.0, ..RectItem::new(r,
                    hex_color(if self.frame_changes == changes { 0x343838 } else { 0x1b1f22 })) }));
                ui_label(items, cx, cy, 14.0, if self.frame_changes == changes || hovered { hex_color(CUT_INK) } else { soft },
                    &control.label, Align::Center, r.w);
            }
            CutAction::TimeDisplay(frames) => {
                ui_label(items, r.x, cy, 10.0, if self.cut_frames == frames { hex_color(CUT_INK) } else { mix(CUT_BAR, 0xffffff, 0.3) },
                    &control.label, Align::Left, r.w);
            }
            CutAction::Snap(_) => {
                let on = cut.snap == Snap::Keyframe;
                if on || hovered {
                    items.push(Item::Rect(RectItem { radius: 6.0, border_w: if on { 1.0 } else { 0.0 },
                        border_color: hex_color(FRAME_KEY), ..RectItem::new(r,
                            hex_color(if on { 0x082b16 } else { 0x17191b })) }));
                }
                draw_icon(items, Icon::SnapKeyframe, cx, cy, hex_color(if on { FRAME_KEY } else { 0xececec }));
            }
            CutAction::EncodeAll => {
                let disabled = cut.encoding() || cut.scanning() || cut.exporting() || cut.pending_gops().is_empty();
                let border = if hovered && !disabled { 0x555555 } else { 0x303030 };
                items.push(Item::Rect(RectItem { radius: 7.0, border_w: 1.0,
                    border_color: hex_color(border), ..RectItem::new(r, hex_color(0x050506)) }));
                let key = RectPx { x: r.x + r.w - 35.0, y: cy - 9.0, w: 29.0, h: 18.0 };
                items.push(Item::Rect(RectItem { radius: 5.0, ..RectItem::new(key, hex_color(0x1b1b1b)) }));
                let ink = hex_color(if disabled { 0x777777 } else { 0xc8c8c8 });
                ui_label(items, r.x + 10.0, cy, 12.0, ink, &control.label, Align::Left, r.w - 48.0);
                ui_label(items, key.x + key.w / 2.0, cy, 10.0, ink, control.hint, Align::Center, key.w);
            }
            CutAction::Encode(_) => {
                let disabled = cut.encoding() || cut.scanning() || cut.exporting() || cut.pending_gops().is_empty();
                items.push(Item::Rect(RectItem { radius: 6.0, ..RectItem::new(r,
                    hex_color(if disabled { 0x302839 } else if hovered { 0xd2b4ff } else { 0xb68aff })) }));
                ui_label(items, cx, cy, 10.5, hex_color(if disabled { 0x8c7e9c } else { 0x28133f }), &control.label, Align::Center, r.w - 6.0);
            }
            CutAction::Export => {
                let busy = cut.exporting() || cut.encoding() || cut.scanning();
                let fill = if busy { mix(0x050506, CUT_INK, 0.35) } else if hovered { hex_color(0xffffff) } else { hex_color(CUT_INK) };
                items.push(Item::Rect(RectItem { radius: 7.0, ..RectItem::new(r, fill) }));
                ui_label(items, cx, cy, 11.5, hex_color(CUT_BG), &control.label, Align::Center, r.w - 8.0);
            }
            CutAction::Undo => {
                let dead = !cut.can_undo();
                items.push(Item::Rect(RectItem { radius: 7.0, border_w: 1.0,
                    border_color: mix(0x050506, 0xffffff, if hovered && !dead { 0.3 } else { 0.14 }),
                    ..RectItem::new(r, if hovered && !dead { mix(0x050506, 0xffffff, 0.05) } else { [0.0; 4] }) }));
                let color = if dead { mix(0x050506, 0xffffff, 0.3) } else { soft };
                let cap: f32 = 22.0;
                ui_label(items, r.x + 13.0, cy, 11.5, color, &control.label, Align::Left, 32.0);
                let key = RectPx { x: r.x + r.w - 13.0 - cap.max(20.0), y: cy - 10.0, w: cap.max(20.0), h: 20.0 };
                items.push(Item::Rect(RectItem { radius: 5.0, ..RectItem::new(key, mix(0x050506, 0xffffff, 0.09)) }));
                ui_label(items, key.x + key.w / 2.0, cy, 10.0, color, control.hint, Align::Center, key.w);
            }
            CutAction::Play => {
                // Hovered: a lighter blue with a hairline ring.
                let (fill, ring) = if hovered { (mix(0x1580de, 0xffffff, 0.16), 1.0) } else { (ACCENT, 0.0) };
                items.push(Item::Rect(RectItem { radius: 17.0, border_w: ring, border_color: mix(0x1580de, 0xffffff, 0.45), ..RectItem::new(r, fill) }));
                let black = hex_color(0x000000);
                if self.playing {
                    for x in [cx - 5.0, cx + 2.0] {
                        items.push(Item::Rect(RectItem::new(RectPx { x, y: cy - 6.0, w: 3.0, h: 12.0 }, black)));
                    }
                } else {
                    items.push(Item::Triangle { r: RectPx { x: cx - 3.5, y: cy - 6.0, w: 10.0, h: 12.0 }, color: black, left: false, radius: 1.0 });
                }
            }
            CutAction::Zoom(zoom_in) => {
                // The board's magnifier: a 10px ring with a − / + inside and a
                // handle running off to the lower right, white at 50%.
                let color = if hovered { hex_color(CUT_INK) } else { mix(CUT_BAR, 0xffffff, 0.5) };
                draw_icon(items, if zoom_in { Icon::ZoomIn } else { Icon::ZoomOut }, cx, cy, color);
            }
            _ => {
                let dead = action == CutAction::Apply && cut.selection().is_none() && self.cut_picks.is_empty() && self.cut_restore.is_none();
                let selected = action == CutAction::ToggleSplit && self.split_tool;
                let fill = if selected { hex_color(CUT_TOOL_ON) } else if hovered && !dead { mix(CUT_BAR, 0xffffff, 0.07) } else { [0.0; 4] };
                if selected || (hovered && !dead) {
                    items.push(Item::Rect(RectItem { radius: 7.0, ..RectItem::new(r, fill) }));
                }
                // Every toolbar icon is plain white; only a dead one dims.
                let white = hex_color(0xffffff);
                let (icon, color) = match action {
                    CutAction::Step(d) => (if d < 0 { Icon::StepBack } else { Icon::StepFwd }, white),
                    CutAction::Keyframe(d) => (if d < 0 { Icon::KeyBack } else { Icon::KeyFwd }, white),
                    CutAction::In => (Icon::In, white),
                    CutAction::Out => (Icon::Out, white),
                    CutAction::Split => (Icon::Split, white),
                    CutAction::ToggleSplit => (Icon::Split, if self.split_tool { ACCENT } else { white }),
                    CutAction::Apply | CutAction::DeletePicked => (Icon::Trash, white),
                    _ => (Icon::SnapKeyframe, white),
                };
                draw_icon(items, icon, cx, cy, if dead { mix(CUT_BG, 0xffffff, 0.25) } else { color });
            }
        }
    }

    fn info_scroll_max(&self) -> f32 {
        let Some(cut) = &self.cut else { return 0.0 };
        let section = |rows: usize| 38.0 + rows as f32 * 22.3;
        let mut height = section(if cut.facts.video.is_empty() { 2 } else { cut.facts.video.len() });
        if !cut.facts.audio_title.is_empty() { height += section(cut.facts.audio.len()); }
        if !cut.facts.subs_title.is_empty() || !cut.cues.is_empty() { height += section(1); }
        height += section(if cut.gop_stats().is_some() { 3 } else { 1 });
        if cut.gops.iter().any(|g| g.start <= self.t + 1e-6) { height += section(4) + 16.0; }
        (height - self.cut_side().body.h).max(0.0)
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
        for (i, (r, icon)) in g.tab.iter().zip([Icon::Flag, Icon::Subtitle, Icon::Segments, Icon::Info]).enumerate() {
            let on = self.cut_tab == [0, 2, 3, 1][i];
            let hovered = self.cursor_inside && contains(*r, self.cursor.0, self.cursor.1);
            if on { items.push(Item::Rect(RectItem { radius: 7.0, border_w: 1.0, border_color: mix(CUT_TOOL_ON, 0x0088ff, 0.35), ..RectItem::new(*r, hex_color(CUT_TOOL_ON)) })); }
            draw_icon(items, icon, r.x + r.w / 2.0, r.y + r.h / 2.0, if on { ACCENT } else if hovered { ink } else { dim });
        }

        let kv = |items: &mut Vec<Item>, y: f32, key: &str, value: String, color: [f32; 4]| {
            inspector_label(items, x0, y, 11.5, dim, key, Align::Left, 110.0);
            items.push(Item::Text(TextItem { header: true, align: Align::Right, valign: VAlign::Middle, max_width: Some(g.body.w - 80.0),
                ..TextItem::new(x1, y, 11.5, color, value) }));
        };

        if self.cut_tab == 0 {
            // ---- chapters ----
            let cy = g.count_y + 13.0;
            inspector_label(items, x0, cy, 11.0, ink, format!("Chapters {}", cut.chapters.len()), Align::Left, g.add_btn.x - x0 - 8.0);
            let plus_hot = self.cursor_inside && contains(g.add_btn, self.cursor.0, self.cursor.1);
            items.push(Item::Rect(RectItem { radius: 7.0, ..RectItem::new(g.add_btn, mix(CUT_BG, 0xffffff, if plus_hot { 0.12 } else { 0.07 })) }));
            let (px, py) = (g.add_btn.x + g.add_btn.w / 2.0, g.add_btn.y + g.add_btn.h / 2.0);
            let plus = if plus_hot { ink } else { dim };
            items.push(Item::Rect(RectItem::new(RectPx { x: px - 5.0, y: py - 0.75, w: 10.0, h: 1.5 }, plus)));
            items.push(Item::Rect(RectItem::new(RectPx { x: px - 0.75, y: py - 5.0, w: 1.5, h: 10.0 }, plus)));
            items.push(Item::Rect(RectItem { radius: 7.0, ..RectItem::new(g.toggle, mix(CUT_BG, 0xffffff, 0.05)) }));
            for (r, icon, on) in [(g.list_btn, Icon::List, !self.cut_thumbs), (g.grid_btn, Icon::Grid, self.cut_thumbs)] {
                if on { items.push(Item::Rect(RectItem { radius: 5.0, border_w: 1.0, border_color: mix(CUT_TOOL_ON, 0x0088ff, 0.35), ..RectItem::new(r, hex_color(CUT_TOOL_ON)) })); }
                draw_icon(items, icon, r.x + r.w / 2.0, r.y + r.h / 2.0, if on { ACCENT } else { dim });
            }
            let current = cut.chapters.iter().rposition(|c| c.start <= self.t + 1e-6);
            if cut.chapters.is_empty() {
                let msg = if cut.scanning() { "reading chapters …" } else { "no chapters in this file" };
                inspector_label(items, x0 + g.body.w / 2.0, g.body.y + 20.0, 11.0, faint, msg, Align::Center, g.body.w);
            }
            for (r, idx) in self.cut_chapter_rows() {
                let c = &cut.chapters[idx];
                let end = cut.chapters.get(idx + 1).map_or(cut.duration, |n| n.start);
                let on = current == Some(idx);
                let hovered = self.cursor_inside && contains(r, self.cursor.0, self.cursor.1);
                let snap_dot = if cut.on_key(c.start, 1.0 / self.fps.max(1.0)) { hex_color(CUT_GREEN) } else { hex_color(CUT_CORAL) };
                let number = (idx + 1).to_string();
                                let ground = if on { mix(CUT_BG, CUT_AMBER, 0.12) } else if hovered { mix(CUT_BG, 0xffffff, 0.07) } else { hex_color(CUT_LANE) };
                if self.cut_thumbs {
                    items.push(Item::Rect(RectItem { radius: 9.0, border_w: if on { 2.0 } else { 0.0 }, border_color: hex_color(CUT_AMBER), ..RectItem::new(r, ground) }));
                    // The chapter's first frame; until it arrives, the film
                    // ground the picture clip uses.
                    let pic = RectPx { x: r.x + 5.0, y: r.y + 5.0, w: r.w - 10.0, h: 62.0 };
                    match self.cut_thumb.slot_of((c.start * 1000.0).round() as i64) {
                        Some(slot) => items.push(Item::Thumb { r: pic, slot, radius: 5.0, alpha: 1.0, cover: true, clip_poster: false }),
                        None => items.push(Item::Rect(RectItem { radius: 5.0, ..RectItem::new(pic, hex_color(CUT_FILM)) })),
                    }
                    chapter_flag(items, pic.x + 4.0, pic.y + 4.0, 18.0, number, on, false);
                    items.push(Item::Rect(RectItem { radius: 3.5, ..RectItem::new(RectPx { x: pic.x + pic.w - 12.0, y: pic.y + 5.0, w: 7.0, h: 7.0 }, snap_dot) }));
                    inspector_label(items, r.x + 8.0, r.y + 80.0, 12.0, mix(CUT_BG, 0xffffff, 0.88), c.title.as_str(), Align::Left, r.w - 16.0);
                    inspector_label(items, r.x + 8.0, r.y + 95.0, 10.0, faint, format!("{} · {}", fmt_hms(c.start), fmt_span(end - c.start)), Align::Left, r.w - 16.0);
                } else {
                    items.push(Item::Rect(RectItem { radius: 8.0, border_w: if on { 2.0 } else { 0.0 }, border_color: hex_color(CUT_AMBER), ..RectItem::new(r, ground) }));
                    let num = RectPx { x: r.x + 10.0, y: r.y + (r.h - 18.0) / 2.0, w: 20.0, h: 18.0 };
                    chapter_flag(items, num.x, num.y, 18.0, number, on, false);
                    inspector_label(items, r.x + 42.0, r.y + 16.0, 12.0, mix(CUT_BG, 0xffffff, 0.88), c.title.as_str(), Align::Left, r.w - 42.0 - 28.0);
                    inspector_label(items, r.x + 42.0, r.y + 32.0, 10.0, faint, format!("{} · {}", fmt_hms(c.start), fmt_span(end - c.start)), Align::Left, r.w - 42.0 - 28.0);
                    items.push(Item::Rect(RectItem { radius: 3.5, ..RectItem::new(RectPx { x: r.x + r.w - 17.0, y: r.y + r.h / 2.0 - 3.5, w: 7.0, h: 7.0 }, snap_dot) }));
                }
            }
        } else if self.cut_tab >= 2 {
            let (title, count) = if self.cut_tab == 2 { ("Subtitles", cut.cues.len()) } else { ("Segments", cut.segments().len()) };
            inspector_label(items, x0, g.body.y + 10.0, 11.0, ink, format!("{title} {count}"), Align::Left, g.body.w);
            if count == 0 {
                inspector_label(items, x0, g.body.y + 42.0, 11.0, faint,
                    if cut.scanning() { "reading subtitles …" } else { "no text subtitles in this file" }, Align::Left, g.body.w);
            }
            for (r, start, end, title) in self.cut_detail_rows() {
                let on = self.t >= start && self.t < end;
                let hovered = self.cursor_inside && contains(r, self.cursor.0, self.cursor.1);
                items.push(Item::Rect(RectItem { radius: 8.0, border_w: if on { 1.0 } else { 0.0 }, border_color: ACCENT,
                    ..RectItem::new(r, if on { hex_color(CUT_TOOL_ON) } else if hovered { mix(CUT_BG, 0xffffff, 0.07) } else { hex_color(CUT_LANE) }) }));
                inspector_label(items, r.x + 10.0, r.y + 16.0, 12.0, ink, title, Align::Left, r.w - 20.0);
                inspector_label(items, r.x + 10.0, r.y + 32.0, 10.0, faint, format!("{} · {}", fmt_hms(start), fmt_span(end - start)), Align::Left, r.w - 20.0);
            }
        } else {
            // ---- streams ----
            let section = |items: &mut Vec<Item>, y: &mut f32, icon: Icon, title: &str, rows: Vec<(String, String, [f32; 4])>| {
                draw_icon(items, icon, x0 + 8.0, *y + 8.0, dim);
                caps_label(items, x0 + 24.0, *y + 8.0, title);
                *y += 22.0;
                for (k, v, color) in rows {
                    kv(items, *y + 11.0, &k, v, color);
                    *y += 22.3;
                }
                *y += 16.0;
            };
            items.push(Item::Clip(Some(g.body)));
            let body_start = items.len();
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
            let mut key_rows = vec![("Keyframes".into(), if cut.scanning() { "reading …".into() } else { cut.keys.len().to_string() }, ink)];
            if let Some((_, median, longest)) = cut.gop_stats() {
                key_rows.push(("Median GOP".into(), format!("{median:.2} s"), ink));
                key_rows.push(("Longest GOP".into(), format!("{longest:.2} s"), ink));
            }
            section(items, &mut y, Icon::Keyframe, "KEYFRAMES", key_rows);
            if let Some((idx, gop)) = cut.gops.iter().enumerate().rev().find(|(_, g)| g.start <= self.t + 1e-6) {
                section(items, &mut y, Icon::Keyframe, "CURRENT GOP", vec![
                    ("GOP".into(), format!("{} / {}", idx + 1, cut.gops.len()), ink),
                    ("Frames¹".into(), gop.packets.to_string(), ink),
                    ("Duration".into(), format!("{:.3} s", (gop.end - gop.start).max(0.0)), ink),
                    ("Video size".into(), if gop.bytes < 1024 { format!("{} B", gop.bytes) } else if gop.bytes < 1024 * 1024 { format!("{:.1} KiB", gop.bytes as f64 / 1024.0) } else { fmt_size(gop.bytes) }, ink),
                ]);
                inspector_label(items, x0, y, 10.0, faint, "¹ Encoded packets; video payload only", Align::Left, g.body.w);
                y += 16.0;
            }
            let offset = self.info_scroll.min((y - g.body.y - g.body.h).max(0.0));
            for item in &mut items[body_start..] {
                match item {
                    Item::Text(t) => t.y -= offset,
                    Item::Rect(r) => r.r.y -= offset,
                    Item::Triangle { r, .. } | Item::TriangleDown { r, .. } => r.y -= offset,
                    _ => {}
                }
            }
            items.push(Item::Clip(Some(s)));
            let total = y - g.body.y;
            if total > g.body.h && g.body.h > 0.0 {
                let h = (g.body.h * g.body.h / total).max(18.0).min(g.body.h);
                let top = g.body.y + (g.body.h - h) * offset / (total - g.body.h);
                items.push(Item::Rect(RectItem { radius: 1.5,
                    ..RectItem::new(RectPx { x: s.x + s.w - 7.0, y: top, w: 3.0, h }, mix(CUT_BG, 0xffffff, 0.22)) }));
            }
        }

        // ---- the snapping warning, when there is one ----
        if !g.warning.is_empty() {
            let r = RectPx { x: x0, y: g.warning_y, w: x1 - x0, h: g.warning_h };
            items.push(Item::Rect(RectItem { radius: 8.0, border_w: 1.0, border_color: mix(CUT_BG, CUT_CORAL, 0.28),
                ..RectItem::new(r, mix(CUT_BG, CUT_CORAL, 0.10)) }));
            for (i, line) in g.warning.iter().enumerate() {
                inspector_label(items, r.x + 12.0, r.y + 15.0 + i as f32 * 15.0, 10.0, hex_color(CUT_CORAL_INK), line.as_str(), Align::Left, r.w - 24.0);
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
            items.push(Item::Text(TextItem { header: true, align: Align::Right, valign: VAlign::Middle,
                bg: Some(TextBg { radius: 5.0, pad_x: 7.0, pad_y: 5.0, ..TextBg::new(bg) }),
                ..TextItem::new(right - 7.0, cy, 10.0, fg, text) }));
            right - w - 6.0
        };
        if self.cut_tab != 1 {
            inspector_label(items, x0, cy, 15.0, ink, fmt_hms(kept), Align::Left, 110.0);
            let (re_text, re_bg, re_fg) = if frame_mode { ("all re-enc", mix(CUT_BG, CUT_CORAL, 0.14), hex_color(CUT_CORAL_INK)) }
                else { ("0 re-enc", mix(CUT_BG, CUT_GREEN, 0.14), green) };
            let r = right_chip(items, x1, re_text, re_bg, re_fg);
            if removed > 1e-3 {
                right_chip(items, r, &format!("−{}", fmt_span(removed)), chip_bg, mix(CUT_BG, 0xffffff, 0.75));
            }
        } else {
            inspector_label(items, x0, cy, 15.0, ink, fmt_hms(cut.duration), Align::Left, 110.0);
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

        // Once the audio read has finished with nothing to show there is no
        // audio: no sound lane, no icon, no empty blue bar — the ground shows.
        let no_audio = cut.wave.is_empty() && !cut.reading_wave();
        // The Figma track controls occupy a darker 49px strip, with an
        // 8px inset before the media. Keep it separate from the content clip.
        items.push(Item::Rect(RectItem::new(RectPx { x: tl.x, y: tl.y, w: 49.0, h: tl.h }, hex_color(0x0d0d0d))));
        line(items, tl.x + 48.0, tl.y, tl.h, 1.0, hex_color(0x242424));
        let gutter = hex_color(0x626262);
        for (lane, icon) in [(lanes.chapters, Icon::Bookmark),
            (lanes.audio, Icon::TrackSpeaker), (Some(lanes.subs), Icon::Subtitle)] {
            if let Some(r) = lane { draw_icon(items, icon, tl.x + 31.0, r.y + r.h / 2.0, gutter); }
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
                // The board's flag: 18px tall, square on the left where the stem
                // is, a pill on the right — a 3px / 9px corner pair, which is a
                // pill with its left end squared off by a second, narrow rect.
                // Every unselected chapter has the same solid flag and full pole.
                let flag = RectPx { x, y: r.y, w: 0.0, h: 18.0 };
                if lit {
                    line(items, x, flag.y + 15.0, 12.0, 2.0, hex_color(CUT_AMBER));
                } else {
                    line(items, x, flag.y + 10.0, (v.y - flag.y - 10.0).max(10.0), 3.0, hex_color(CUT_FLAG_PAST));
                }
                chapter_flag(items, x, flag.y, 18.0, (i + 1).to_string(), lit, !lit);
            }
        }

        // ---- video and audio: one clip per piece ----
        let segments = cut.segments();
        let thumbs = self.cut_thumb_rects();
        let encode_regions = self.cut_encode_regions();
        // The kept clip under the pointer, picture or sound: a slight lift,
        // eased in and out by `tick_cut_hot`.
        let ease = |l: f32| l * l * (3.0 - 2.0 * l);
        // Draw the picked clips' outlines last, inside the timeline column so
        // a scrolled clip cannot paint over the track-label gutter.
        let mut picked: Vec<RectPx> = Vec::new();
        // A removed clip's (or a missing track's) dashed outline: straight dashes
        // between the corners, and each corner an arc cut from a rounded border by
        // a quadrant scissor — there is no line or arc primitive.
        let dashes = |items: &mut Vec<Item>, r: RectPx, color: [f32; 4]| {
            const RAD: f32 = 7.0;
            let mut x = r.x + RAD;
            while x < r.x + r.w - RAD {
                let w = 4.0_f32.min(r.x + r.w - RAD - x);
                line(items, x, r.y, 1.0, w, color);
                line(items, x, r.y + r.h - 1.0, 1.0, w, color);
                x += 8.0;
            }
            let mut y = r.y + RAD;
            while y < r.y + r.h - RAD {
                let h = 4.0_f32.min(r.y + r.h - RAD - y);
                line(items, r.x, y, h, 1.0, color);
                line(items, r.x + r.w - 1.0, y, h, 1.0, color);
                y += 8.0;
            }
            for (cx, cy) in [(r.x, r.y), (r.x + r.w - RAD, r.y), (r.x, r.y + r.h - RAD), (r.x + r.w - RAD, r.y + r.h - RAD)] {
                items.push(Item::Clip(Some(RectPx { x: cx, y: cy, w: RAD, h: RAD })));
                items.push(Item::Rect(RectItem { radius: RAD, border_w: 1.0, border_color: color, ..RectItem::new(r, [0.0; 4]) }));
            }
            items.push(Item::Clip(Some(column)));
        };
        // Trim highlights must follow the same rounded silhouette as the clip.
        // Scissor slices of a rounded quad, never square strips over its corners.
        let grips = |items: &mut Vec<Item>, r: RectPx, inset: f32| {
            if r.w < 28.0 { return; }
            for i in 0..8 {
                let alpha = 0.035 * (1.0 - i as f32 / 8.0);
                for x in [r.x + i as f32, r.x + r.w - i as f32 - 1.0] {
                    let left = x.max(column.x);
                    let right = (x + 1.0).min(column.x + column.w);
                    if right <= left { continue; }
                    items.push(Item::Clip(Some(RectPx { x: left, y: r.y, w: right - left, h: r.h })));
                    items.push(Item::Rect(RectItem { radius: 7.0,
                        ..RectItem::new(r, [1.0, 1.0, 1.0, alpha]) }));
                }
            }
            items.push(Item::Clip(Some(column)));
            for x in [r.x + 2.55, r.x + 4.89, r.x + r.w - 5.45, r.x + r.w - 3.11] {
                if x > v.x && x < v.x + v.w {
                    line(items, x, r.y + inset, r.h - 2.0 * inset, 0.78, [1.0, 1.0, 1.0, 0.22]);
                }
            }
        };
        for (lane, audio) in [(Some(v), false), (lanes.audio, true)] {
            let Some(row) = lane else { continue };
            for (i, g) in segments.iter().enumerate().filter(|(_, g)| g.end > a && g.start < b) {
                let r = RectPx { x: x_of(g.start) + 1.0, y: row.y, w: (x_of(g.end) - x_of(g.start) - 2.0).max(1.0), h: row.h };
                // No audio: the sound lane stays, as dashes where the clips would be.
                if audio && no_audio {
                    // The missing track: a faint blue ground inside the dashes, like a sound clip's.
                    items.push(Item::Rect(RectItem { radius: 7.0, ..RectItem::new(r, mix(CUT_TL, 0x1580de, 0.07)) }));
                    dashes(items, r, mix(CUT_TL, 0x1580de, 0.2));
                } else if g.cut {
                    dashes(items, r, mix(CUT_TL, 0xffffff, 0.3));
                    if self.cut_restore.is_some_and(|(a,b)| (a-g.start).abs() < 1e-6 && (b-g.end).abs() < 1e-6) {
                        picked.push(r);
                    }
                } else if audio {
                    let hot = ease(self.cut_hot_level(g.start, true));
                    let ground = hex_color(if i % 2 == 1 { CUT_SOUND_ALT } else { CUT_SOUND });
                    items.push(Item::Rect(RectItem { radius: 7.0, ..RectItem::new(r, lerp4(ground, mix(CUT_SOUND, 0x1580de, 0.09), hot)) }));
                    // #112a42 at the top running out to the ground at the foot.
                    items.push(Item::Rect(RectItem { radius: 7.0, fade_down: true, ..RectItem::new(r, hex_color(CUT_SOUND_TOP)) }));
                    items.push(Item::Rect(RectItem { radius: 7.0, border_w: 1.0, border_color: mix(CUT_TL, 0x1580de, 0.6 + 0.15 * hot),
                        ..RectItem::new(r, [0.0; 4]) }));
                    if self.cut_picks.iter().any(|(a, b, snd)| *snd && (a - g.start).abs() < 1e-3 && (b - g.end).abs() < 1e-3) {
                        picked.push(r);
                    }
                } else {
                    // The film ground: the board's 135° stripes (#2a2d30 / #202326, 14px).
                    if !self.frame_markers_visible() && !self.frame_detail() {
                        items.push(Item::Hatch { r, a: hex_color(0x2a2d30), b: hex_color(0x202326), radius: 7.0, period: 14.0 });
                    }
                    if !self.cut_encode_rail() {
                        for (_, region) in &encode_regions {
                            let left = region.x.max(r.x);
                            let right = (region.x + region.w).min(r.x + r.w);
                            if right <= left { continue; }
                            // Tint a slice of the original clip silhouette. Interior
                            // GOP boundaries stay square; only clip ends are rounded.
                            items.push(Item::Clip(Some(RectPx { x: left, y: r.y, w: right-left, h: r.h })));
                            items.push(Item::Hatch { r, a: hex_color(0x59348c), b: hex_color(0x704a9d), radius: 7.0, period: 14.0 });
                            let border = hex_color(0xb68aff);
                            items.push(Item::Rect(RectItem { radius: 7.0, border_w: 2.0, border_color: border, ..RectItem::new(r, [0.0; 4]) }));
                            for edge in [region.x, region.x + region.w - 2.0] {
                                if edge > r.x + 7.0 && edge < r.x + r.w - 7.0
                                    && edge > v.x && edge < v.x + v.w - 2.0 {
                                    line(items, edge, r.y, r.h, 2.0, border);
                                }
                            }
                            items.push(Item::Clip(Some(column)));
                        }
                    }
                    let hot = ease(self.cut_hot_level(g.start, false));
                    items.push(Item::Rect(RectItem { radius: 7.0, border_w: 1.0, border_color: mix(CUT_TL, 0xffffff, 0.2 + 0.06 * hot),
                        ..RectItem::new(r, [1.0, 1.0, 1.0, 0.01 * hot]) }));
                    // The picked clip gets an inset outline after all clip contents.
                    if !self.cut_gop && self.cut_picks.iter().any(|(a, b, snd)| !snd && (a - g.start).abs() < 1e-3 && (b - g.end).abs() < 1e-3) {
                        picked.push(r);
                    }
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
                if !audio && !g.cut {
                    let key = (g.start * 1000.0).round() as i64;
                    if let (Some((_, _, tr)), Some(slot)) = (thumbs.iter().find(|t| t.0 == key), self.cut_thumb.slot_of(key)) {
                        items.push(Item::Thumb { r: *tr, slot, radius: 6.0, alpha: 1.0, cover: true, clip_poster: true });
                        if let Some(p) = self.cut_thumb.play.as_ref().filter(|p| p.key == key) {
                            let frac = (((p.t - p.start) / (p.end - p.start).max(1e-6)) as f32).clamp(0.0, 1.0);
                            let bar = RectPx { x: tr.x + 1.0, y: tr.y + tr.h - 4.0, w: tr.w - 2.0, h: 3.0 };
                            items.push(Item::Rect(RectItem::new(bar, [0.0, 0.0, 0.0, 0.55])));
                            items.push(Item::Rect(RectItem::new(RectPx { w: (bar.w * frac).max(1.0), ..bar }, hex_color(CUT_IN))));
                        }
                    }
                }
                if audio && !cut.wave.is_empty() {
                    let (from, to) = ((r.x + 12.0).max(v.x), (r.x + r.w - 12.0).min(v.x + v.w));
                    let mut x = from;
                    while x < to {
                        let ta = t0 + ((x - v.x) as f64) / pps;
                        let Some(e) = cut.peak(ta, ta + 3.0 / pps) else { break };
                        let h = (e.sqrt() * (r.h - 10.0)).max(6.0).min(r.h - 10.0).round();
                        items.push(Item::Rect(RectItem { radius: 1.0, ..RectItem::new(
                            RectPx { x, y: r.y + ((r.h - h) / 2.0).round(), w: 2.0, h },
                            mix(if g.cut { CUT_TL } else { CUT_SOUND }, CUT_WAVE, if g.cut { 0.22 } else { 0.95 })) }));
                        x += 3.0;
                    }
                }
                if !g.cut && !(audio && no_audio) { grips(items, r, if audio { 6.0 } else { 9.0 }); }
            }
            if audio && cut.wave.is_empty() && cut.reading_wave() {
                ui_label(items, row.x + 16.0, row.y + row.h / 2.0, 10.0, mix(CUT_SOUND, 0xffffff, 0.4), "reading audio…", Align::Left, row.w);
            }
        }

        // Actual presentation timestamps, not nominal FPS: VFR clips get the
        // correct markers too. Binary search bounds the work to visible frames.
        let lo = cut.frames.partition_point(|t| *t < a).saturating_sub(1);
        let hi = cut.frames.partition_point(|t| *t <= b);
        let visible = &cut.frames[lo..hi];
        if !self.frame_detail() && self.frame_markers_visible() {
            for (i, t) in visible.iter().enumerate() {
                let index = lo+i;
                let Some(segment) = segments.iter().find(|s| !s.cut && *t >= s.start && *t < s.end) else { continue };
                let end = cut.frames.get(index+1).copied().unwrap_or(cut.duration).min(segment.end);
                let spacing = ((end - *t)*pps) as f32;
                if spacing < 10.0 { continue; }
                let fi = cut.frame_info.partition_point(|f| f.pts < *t - 1e-6);
                let info = cut.frame_info.get(fi).filter(|f| (f.pts-*t).abs() < 1e-6);
                let key = info.map_or_else(|| cut.keys.binary_search_by(|k| k.total_cmp(t)).is_ok(), |f| f.key);
                let left = x_of(segment.start).max(v.x);
                let right = x_of(segment.end).min(v.x + v.w);
                items.push(Item::Clip(Some(RectPx { x: left, y: v.y, w: (right-left).max(0.0), h: v.h })));
                draw_frame_marker(items, RectPx { x: x_of(*t), y: v.y, w: spacing, h: v.h },
                    info.map_or('?', |f| f.kind), key, index);
                items.push(Item::Clip(Some(column)));
            }
        }

        if self.frame_detail() { self.build_frame_detail(items, column); }

        for (_, r) in encode_regions {
            if self.cut_encode_rail() {
                items.push(Item::Rect(RectItem { radius: 3.0, ..RectItem::new(r, hex_color(0x59348c)) }));
                items.push(Item::Rect(RectItem { radius: 5.0, border_w: 1.0, border_color: hex_color(0xb68aff), ..RectItem::new(r, [0.0; 4]) }));
            }
            if r.w >= 54.0 {
                let button = Self::encode_button(r);
                items.push(Item::Rect(RectItem { radius: 4.0, ..RectItem::new(button, hex_color(0xe4d3ff)) }));
                ui_label(items, button.x + button.w / 2.0, button.y + button.h / 2.0, 10.0, hex_color(0x28133f),
                    if cut.encoding() { "…" } else { "Encode" }, Align::Center, button.w);
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
            if tall { line(items, x, k.y, k.h, 1.0, hex_color(CUT_HEAD)); }
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
            if left {
                // In: a 5px bar with a 9×6 foot off its left at the top and the bottom.
                items.push(Item::Rect(RectItem { radius: 2.0, ..RectItem::new(RectPx { x: x - 2.5, y: top, w: 5.0, h: bottom - top }, hex_color(color)) }));
                for y in [top, bottom - 6.0] {
                    items.push(Item::Rect(RectItem { radius: 3.0, ..RectItem::new(RectPx { x: x - 2.5 - 7.8, y, w: 9.0, h: 6.0 }, hex_color(color)) }));
                    items.push(Item::Rect(RectItem::new(RectPx { x: x - 2.5 - 3.0, y, w: 3.0, h: 6.0 }, hex_color(color))));
                }
            } else {
                // Out: a 3px bar with an 11×16 tab off its right at the top.
                items.push(Item::Rect(RectItem { radius: 1.5, ..RectItem::new(RectPx { x: x - 1.5, y: top, w: 3.0, h: bottom - top }, hex_color(color)) }));
                items.push(Item::Rect(RectItem { radius: 4.0, ..RectItem::new(RectPx { x: x - 1.5, y: top, w: 11.0, h: 16.0 }, hex_color(color)) }));
                items.push(Item::Rect(RectItem::new(RectPx { x: x - 1.5, y: top, w: 4.0, h: 16.0 }, hex_color(color))));
            }
        }
        if let Some(at) = self.cut_razor() {
            let x = x_of(at);
            let razor_bottom = lanes.audio.map_or(v.y + v.h, |r| r.y + r.h);
            dashed(items, x - 0.75, v.y - 8.0, razor_bottom, 1.5, hex_color(CUT_INK));
            items.push(Item::Rect(RectItem { radius: 14.0, ..RectItem::new(RectPx { x: x - 14.0, y: v.y - 34.0, w: 28.0, h: 28.0 }, hex_color(CUT_INK)) }));
            draw_icon(items, Icon::Split, x, v.y - 20.0, hex_color(CUT_BG));
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
        if let Some(at) = self.cut_hover_playhead() {
            let x = x_of(at);
            let mut ghost = hex_color(CUT_HEAD);
            ghost[3] = 0.22;
            line(items, x - 0.5, tl.y + 1.0, tl.h - 1.0, 1.0, ghost);
            items.push(Item::Rect(RectItem { radius: 1.2, ..RectItem::new(
                RectPx { x: x - 5.5, y: tl.y - 2.0, w: 11.0, h: 7.5 }, ghost) }));
            items.push(Item::TriangleDown { r: RectPx { x: x - 5.5, y: tl.y + 5.3, w: 11.0, h: 5.7 }, color: ghost, radius: 0.3 });
            for (gx, h) in [(x - 2.15, 4.1), (x - 0.4, 5.25), (x + 1.35, 4.1)] {
                line(items, gx, tl.y + 1.0, h, 0.8, [0.0, 0.0, 0.0, 0.22]);
            }
        }
        let px = x_of(self.t);
        let head = hex_color(CUT_HEAD);
        // One white pixel, with a pixel of translucent black either side so it
        // holds against pale and dark footage alike.
        line(items, px - 1.5, tl.y + 1.0, tl.h - 1.0, 1.0, [0.0, 0.0, 0.0, 0.3]);
        line(items, px + 0.5, tl.y + 1.0, tl.h - 1.0, 1.0, [0.0, 0.0, 0.0, 0.3]);
        line(items, px - 0.5, tl.y + 1.0, tl.h - 1.0, 1.0, head);
        // The pin: the board's 11px tab, square for 7px then a point to 13px,
        // three dark grooves to take hold of.
        items.push(Item::Rect(RectItem { radius: 1.2, ..RectItem::new(RectPx { x: px - 5.5, y: tl.y - 2.0, w: 11.0, h: 7.5 }, head) }));
        items.push(Item::TriangleDown { r: RectPx { x: px - 5.5, y: tl.y + 5.3, w: 11.0, h: 5.7 }, color: head, radius: 0.3 });
        for (x, h) in [(px - 2.15, 4.1), (px - 0.4, 5.25), (px + 1.35, 4.1)] {
            line(items, x, tl.y + 1.0, h, 0.8, mix(CUT_HEAD, 0x0b0c0d, 0.55));
        }
        // The timecode rides the ruler row, centred under the line: white, black type.
        let tc = fmt_tc(self.t, self.fps);
        let chip = if cut.duration >= 3600.0 { tc.as_str() } else { &tc[3..] };
        let half = chip.chars().count() as f32 * 10.5 * MONO_ADV / 2.0 + 7.0;
        let cx = px.clamp(v.x + half, v.x + v.w - half);
        items.push(Item::Text(TextItem { align: Align::Center, valign: VAlign::Middle,
            bg: Some(TextBg { radius: 5.0, pad_x: 7.0, pad_y: 1.5, ..TextBg::new(hex_color(0xffffff)) }),
            ..TextItem::new(cx, lanes.keys.y + 5.5, 10.5, hex_color(0x000000), chip) }));
        for r in picked {
            // The renderer places the band inside this shape. Keeping the clip's
            // own bounds prevents the gutter and neighbouring clips cutting it off.
            items.push(Item::Rect(RectItem { radius: 7.0, border_w: 2.0, border_color: hex_color(0xffffff), ..RectItem::new(r, [0.0; 4]) }));
        }
        // The rubber band: a faint accent wash with a 1px accent edge.
        if let Some(b) = self.cut_band_rect() {
            items.push(Item::Rect(RectItem::new(b, [ACCENT[0], ACCENT[1], ACCENT[2], 0.18])));
            items.push(Item::Rect(RectItem { border_w: 1.0, border_color: ACCENT, ..RectItem::new(b, [0.0; 4]) }));
        }
        items.push(Item::Clip(None));
    }
}

/// The scrub panel's pictograms. The renderer draws rects and triangles,
/// not paths, so each is built from those on a 16px box about its centre.
#[derive(Clone, Copy)]
enum Icon { Info, Keyframe, Bookmark, TrackFilm, TrackSpeaker, Subtitle, Segments, In, Out, Split, Trash, SnapKeyframe, Flag, Film, Speaker, Caption, List, Grid, StepBack, StepFwd, KeyBack, KeyFwd, ZoomOut, ZoomIn }

/// Figma frame/type palette. Key status is independent of picture type.
const FRAME_KEY: u32 = 0x00ff26;
const FRAME_I: u32 = 0xf6ff00;
const FRAME_P: u32 = 0xff00d5;
const FRAME_B: u32 = 0x00d0ff;

fn frame_color(kind: char, key: bool) -> [f32; 4] {
    hex_color(if key {FRAME_KEY} else {match kind {'I'=>FRAME_I,'P'=>FRAME_P,'B'=>FRAME_B,_=>0x969da5}})
}

/// Cells appear at close zoom; medium zoom uses bottom-aligned frame-type ticks.
fn draw_frame_marker(items: &mut Vec<Item>, r: RectPx, kind: char, key: bool, index: usize) {
    if r.w < 10.0 { return; }
    let color = hex_color(if key { FRAME_KEY } else {
        match kind { 'I' => FRAME_I, 'P' => FRAME_P, 'B' => FRAME_B, _ => 0x969da5 }
    });
    if r.w < 32.0 {
        // Match the design's short B / medium P / tall keyframe rhythm.
        // Leave a little air below the shared baseline and above the tallest tick.
        let h: f32 = if key { 32.0 } else { match kind { 'I' => 26.0, 'P' => 20.0, _ => 12.0 } };
        let h = h.min((r.h - 10.0).max(0.0));
        items.push(Item::Rect(RectItem::new(
            RectPx { x: r.x + 1.0, y: r.y + r.h - 5.0 - h, w: 2.0, h }, color)));
        return;
    }
    let cell = RectPx { x: r.x + 1.0, y: r.y + 1.0, w: (r.w - 2.0).max(1.0), h: r.h - 2.0 };
    if r.w >= 32.0 {
        items.push(Item::Rect(RectItem { radius: 4.0, border_w: 1.0, border_color: color,
            ..RectItem::new(cell, hex_color(0x1b1f22)) }));
    }
    items.push(Item::Rect(RectItem { radius: 2.0, ..RectItem::new(
        RectPx { w: if r.w >= 32.0 { 5.0 } else { 2.0 }, ..cell }, color) }));
    if r.w >= 32.0 {
        // Geometric diamond rather than a font glyph: fonts need not contain it.
        if key {
            for (dy, width) in [(0.0, 1.0), (1.0, 3.0), (2.0, 5.0), (3.0, 3.0), (4.0, 1.0)] {
                for x in [cell.x + 9.0 - width / 2.0, cell.x + 9.0 + width / 2.0 - 1.0] {
                    items.push(Item::Rect(RectItem::new(RectPx { x, y: cell.y + 8.0 + dy, w: 1.0, h: 1.0 }, color)));
                }
            }
        }
        items.push(Item::Text(TextItem { max_width: Some(r.w - 16.0),
            ..TextItem::new(cell.x + if key { 16.0 } else { 8.0 }, cell.y + 6.0, 8.0, color, kind.to_string()) }));
    }
    if r.w >= 80.0 {
        items.push(Item::Text(TextItem { align: Align::Right, max_width: Some(r.w - 34.0),
            ..TextItem::new(cell.x + cell.w - 8.0, cell.y + 6.0, 8.0, hex_color(0x969da5), format!("{index:03}")) }));
    }
}

fn draw_icon(items: &mut Vec<Item>, icon: Icon, cx: f32, cy: f32, color: [f32; 4]) {
    let bar = |items: &mut Vec<Item>, x: f32, y: f32, w: f32, h: f32| {
        items.push(Item::Rect(RectItem { radius: 0.75, ..RectItem::new(RectPx { x: cx + x, y: cy + y, w, h }, color) }));
    };
    let stroke = |items: &mut Vec<Item>, a: (f32, f32), b: (f32, f32)| {
        let steps = ((b.0 - a.0).abs().max((b.1 - a.1).abs()) * 3.0).ceil() as usize;
        for i in 0..=steps {
            let t = i as f32 / steps.max(1) as f32;
            bar(items, a.0 + (b.0 - a.0) * t - 0.65, a.1 + (b.1 - a.1) * t - 0.65, 1.3, 1.3);
        }
    };
    match icon {
        Icon::Bookmark => {
            items.push(Item::Rect(RectItem { radius: 1.6, border_w: 1.0, border_color: color,
                ..RectItem::new(RectPx { x: cx - 6.0, y: cy - 6.0, w: 12.0, h: 12.0 }, [0.0; 4]) }));
            for (a, b) in [((-3.3, -6.0), (-3.3, 3.3)), ((-3.3, 3.3), (0.0, 1.6)),
                ((0.0, 1.6), (3.3, 3.3)), ((3.3, 3.3), (3.3, -6.0))] { stroke(items, a, b); }
        }
        Icon::TrackFilm => {
            items.push(Item::Rect(RectItem { radius: 0.75, border_w: 1.0, border_color: color,
                ..RectItem::new(RectPx { x: cx - 6.0, y: cy - 5.0, w: 12.0, h: 10.0 }, [0.0; 4]) }));
            for x in [-3.7, 2.7] { bar(items, x, -5.0, 1.0, 10.0); }
            for y in [-2.5, 0.0, 2.5] {
                bar(items, -6.0, y - 0.5, 2.8, 1.0);
                bar(items, 3.2, y - 0.5, 2.8, 1.0);
            }
        }
        Icon::TrackSpeaker => {
            for (a, b) in [((-5.5, -2.3), (-3.5, -2.3)), ((-3.5, -2.3), (-0.5, -5.5)),
                ((-0.5, -5.5), (-0.5, 5.5)), ((-0.5, 5.5), (-3.5, 2.3)),
                ((-3.5, 2.3), (-5.5, 2.3)), ((-5.5, 2.3), (-5.5, -2.3))] { stroke(items, a, b); }
            for (x, radius) in [(1.5, 2.5), (3.2, 4.3)] {
                for i in 0..12 {
                    let a = -0.8 + i as f32 * 1.6 / 12.0;
                    let b = -0.8 + (i + 1) as f32 * 1.6 / 12.0;
                    stroke(items, (x + radius * (a.cos() - 0.7), radius * a.sin()),
                        (x + radius * (b.cos() - 0.7), radius * b.sin()));
                }
            }
        }
        Icon::Subtitle => {
            items.push(Item::Rect(RectItem { radius: 1.0, border_w: 1.0, border_color: color,
                ..RectItem::new(RectPx { x: cx - 6.0, y: cy - 5.5, w: 12.0, h: 11.0 }, [0.0; 4]) }));
            stroke(items, (-2.5, 2.5), (0.0, -2.5));
            stroke(items, (0.0, -2.5), (2.5, 2.5));
            bar(items, -1.7, 0.4, 3.4, 1.0);
        }
        // An arrow running into a bracket: the range starts / ends here.
        Icon::In | Icon::Out => {
            let m = if matches!(icon, Icon::In) { 1.0 } else { -1.0 };
            let at = |x: f32, w: f32| if m > 0.0 { x } else { -x - w };
            bar(items, at(0.0, 1.5), -8.0, 1.5, 16.0);
            bar(items, at(0.0, 6.0), -8.0, 6.0, 1.5);
            bar(items, at(0.0, 6.0), 6.5, 6.0, 1.5);
            bar(items, at(-8.0, 6.0), -0.75, 6.0, 1.5);
            stroke(items, (-5.0 * m, -3.5), (-1.5 * m, 0.0));
            stroke(items, (-1.5 * m, 0.0), (-5.0 * m, 3.5));
        }
        // Two outward arrows around a dashed playhead, matching the supplied icon.
        Icon::Split => {
            for y in [-8.0, -2.0, 4.0] { bar(items, -0.75, y, 1.5, 4.0); }
            for (x, left) in [(-8.0, true), (3.0, false)] {
                items.push(Item::Triangle { r: RectPx { x: cx + x, y: cy - 4.0, w: 5.0, h: 8.0 }, color, left, radius: 0.0 });
            }
        }
        Icon::Trash => {
            stroke(items, (-5.5, -5.5), (5.5, 5.5));
            stroke(items, (5.5, -5.5), (-5.5, 5.5));
        }
        // A keyframe diamond locked to a vertical snap line.
        Icon::SnapKeyframe => {
            bar(items, -0.75, -8.0, 1.5, 16.0);
            for (size, ink) in [(5.7, color), (4.3, hex_color(CUT_BAR))] {
                for (x, left) in [(-size, true), (0.0, false)] {
                    items.push(Item::Triangle { r: RectPx { x: cx + x, y: cy - size, w: size, h: size * 2.0 }, color: ink, left, radius: 0.0 });
                }
            }
        }
        Icon::Segments => {
            for (x, w) in [(-7.0, 5.0), (0.0, 7.0)] {
                items.push(Item::Rect(RectItem { radius: 1.5, border_w: 1.0, border_color: color,
                    ..RectItem::new(RectPx { x: cx + x, y: cy - 5.0, w, h: 10.0 }, [0.0; 4]) }));
            }
        }
        Icon::Flag => {
            bar(items, -4.0, -6.5, 1.0, 13.0);
            bar(items, -3.0, -5.5, 7.0, 1.0);
            bar(items, -3.0, 0.0, 7.0, 1.0);
            stroke(items, (3.5, -5.0), (1.5, -2.5));
            stroke(items, (1.5, -2.5), (3.5, 0.5));
        }
        Icon::Film => {
            items.push(Item::Rect(RectItem { radius: 1.0, border_w: 1.0, border_color: color,
                ..RectItem::new(RectPx { x: cx - 7.0, y: cy - 5.5, w: 14.0, h: 11.0 }, [0.0; 4]) }));
            bar(items, -4.0, -5.5, 1.0, 11.0);
            bar(items, 3.0, -5.5, 1.0, 11.0);
            bar(items, -7.0, -0.5, 14.0, 1.0);
            for y in [-3.0, 2.0] {
                bar(items, -7.0, y, 3.0, 1.0);
                bar(items, 4.0, y, 3.0, 1.0);
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
        Icon::Info => {
            items.push(Item::Rect(RectItem { radius: 7.5, border_w: 1.3, border_color: color,
                ..RectItem::new(RectPx { x: cx - 7.5, y: cy - 7.5, w: 15.0, h: 15.0 }, [0.0; 4]) }));
            bar(items, -0.75, -4.0, 1.5, 1.5);
            bar(items, -0.75, -0.5, 1.5, 4.5);
        }
        Icon::Keyframe => {
            let points = [(0.0, -7.0), (5.0, 0.0), (0.0, 7.0), (-5.0, 0.0), (0.0, -7.0)];
            for edge in points.windows(2) { stroke(items, edge[0], edge[1]); }
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
            let center = -3.5 * m;
            let points = [(center, -6.0), (center + 4.0, 0.0), (center, 6.0), (center - 4.0, 0.0), (center, -6.0)];
            for edge in points.windows(2) { stroke(items, edge[0], edge[1]); }
            stroke(items, (3.0 * m, -5.0), (7.0 * m, 0.0));
            stroke(items, (7.0 * m, 0.0), (3.0 * m, 5.0));
        }
        Icon::List => {
            for y in [-3.5, -0.5, 2.5] { bar(items, -5.0, y, 10.0, 1.0); }
        }
        // A magnifier: a 10px ring about (−1, −1) with a − or + inside, and a
        // handle running to the lower right — a stair of 1.5px squares, the
        // nearest thing to a diagonal the rect primitive allows.
        Icon::ZoomOut | Icon::ZoomIn => {
            items.push(Item::Rect(RectItem { radius: 5.0, border_w: 1.1, border_color: color,
                ..RectItem::new(RectPx { x: cx - 6.0, y: cy - 6.0, w: 10.0, h: 10.0 }, [0.0; 4]) }));
            bar(items, -3.0, -1.55, 4.0, 1.1);
            if matches!(icon, Icon::ZoomIn) { bar(items, -1.55, -3.0, 1.1, 4.0); }
            for i in 0..3 {
                let d = 2.6 + i as f32 * 1.2;
                bar(items, d - 0.7, d - 0.7, 1.4, 1.4);
            }
        }
        Icon::Grid => {
            for (x, y) in [(-6.5, -6.5), (0.5, -6.5), (-6.5, 0.5), (0.5, 0.5)] {
                items.push(Item::Rect(RectItem { radius: 1.5, border_w: 1.0, border_color: color, ..RectItem::new(RectPx { x: cx + x, y: cy + y, w: 5.5, h: 5.5 }, [0.0; 4]) }));
            }
        }
    }
}

/// The design's small-caps section label.
fn inspector_label(items: &mut Vec<Item>, x: f32, y: f32, px: f32, color: [f32; 4], text: impl Into<String>, align: Align, width: f32) {
    let mono = px == 10.0 || px == 15.0;
    items.push(Item::Text(TextItem { inspector: if mono { 0 } else if px == 12.0 { 2 } else { 1 }, header: mono,
        tracking: if mono { 0.0 } else { 0.0337 }, valign: VAlign::Middle, align, max_width: Some(width),
        ..TextItem::new(x, y, px, color, text) }));
}

fn caps_label(items: &mut Vec<Item>, x: f32, y: f32, text: &str) {
    items.push(Item::Text(TextItem { inspector: 3, valign: VAlign::Middle, tracking: 0.9323,
        ..TextItem::new(x, y, 10.5, mix(CUT_BG, 0xffffff, 0.42), text) }));
}

/// `fg` at `alpha` over `bg`, composited in sRGB — the design's numbers.
/// The renderer blends in linear light, where the same alpha reads far
/// brighter, so translucent design tokens over a KNOWN ground are baked
/// to the opaque colour they were meant to produce.
/// Linear blend of two premixed colours by `t` (0 = `a`, 1 = `b`).
fn lerp4(a: [f32; 4], b: [f32; 4], t: f32) -> [f32; 4] {
    std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t)
}

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
const CUT_TIMELINE_H: f32 = 198.0;
/// Short windows drop the chapter and audio rows.
const CUT_TIMELINE_COMPACT_H: f32 = 126.0;
const CUT_FULL_MIN_H: f32 = 640.0;
const CUT_SIDE_W: f32 = 300.0;
/// The icon gutter left of the rows, and the pad right of them.
const CUT_GUTTER: f32 = 56.0;
const CUT_PAD: f32 = 16.0;
/// How far from a clip's edge a press still takes hold of it: the drawn
/// grip sits 3–7px inside the edge, so this covers it with room to spare.
const CUT_GRIP: f32 = 12.0;
// The Timeline Edit design's tokens, sRGB hex (its oklch values converted).
const CUT_BG: u32 = 0x0e0f10;
/// Timeline preview surround from Figma 12:42.
const CUT_CANVAS: u32 = 0x2c2c2c;
/// Stacked current-chapter tag and transport readout (35:24 / 73:126).
const CUT_CHAPTER_TAG: u32 = 0xd00070;
/// The selected Figma transport/toolbar uses a black ground.
const CUT_BAR: u32 = 0x000000;
/// The lit tab / toggle ground: the app's TOOL_BG.
const CUT_TOOL_ON: u32 = 0x0b1926;
const CUT_TL: u32 = 0x121212;
/// The inspector's segment rows.
const CUT_LANE: u32 = 0x151719;
/// A kept piece of picture, and of sound (alternating, so neighbours part).
const CUT_FILM: u32 = 0x25282b;
const CUT_SOUND: u32 = 0x0c1d2f;
const CUT_SOUND_ALT: u32 = 0x0f2438;
/// The top of a sound clip's vertical gradient (#112a42 → `CUT_SOUND`).
const CUT_SOUND_TOP: u32 = 0x112a42;
const CUT_WAVE: u32 = 0x5db0f5;
/// In is blue, out is red; the cut button a lighter red.
const CUT_IN: u32 = 0x4aa3ee;
const CUT_OUT: u32 = 0xff6a72;
/// The playhead and the keyframes the selection snapped to.
const CUT_HEAD: u32 = 0xf2f3f4;
const CUT_INK: u32 = 0xededec;
/// Chapters and the keyframe chip.
const CUT_AMBER: u32 = 0xe9ac00;
const CUT_AMBER_INK: u32 = 0x1a1408;
/// Lossless / snapped.
const CUT_GREEN: u32 = 0x35c15f;
const CUT_GREEN_INK: u32 = 0x7fdc98;
/// Warnings only: long GOPs, what snapping costs, the cut button.
const CUT_CORAL: u32 = 0xf08566;
const CUT_CORAL_INK: u32 = 0xf5b3a0;
const CUT_CUE: u32 = 0xc1a9ee;
/// A chapter flag already passed; the snap lamp.
const CUT_FLAG_PAST: u32 = 0x393a3a;
const CUT_LAMP: u32 = 0x20c45f;

#[derive(Clone, Copy)]
struct Workspace {
    /// The width the source panel occupies this frame (0 while hidden,
    /// in between while it slides).
    rail: f32,
    /// The panel's own width; `list` and `inspector` are this wide and
    /// start at `rail - panel`.
    panel: f32,
    canvas: RectPx,
    list: RectPx,
    /// The visible part of the focused inspector (its content is
    /// `RAIL_INSPECTOR_H` tall, anchored at the top, clipped to this).
    inspector: RectPx,
    /// The grip strip along the inspector's top edge; zero-sized when
    /// the rail has no inspector at all.
    grip: RectPx,
    transport: RectPx,
    /// Cut mode only (zero-sized otherwise): the lanes panel under the
    /// transport and the inspector column right of the canvas.
    timeline: RectPx,
    side: RectPx,
}
impl Workspace {
    fn new(vp: (f32, f32), visible: bool, cut: bool, slide: f32, side_slide: f32, detail_slide: f32) -> Self {
        let (w, h) = vp;
        let none = RectPx { x: 0.0, y: 0.0, w: 0.0, h: 0.0 };
        // `rail` is what the layout gives the panel this frame; `panel` is
        // the panel's own width, and its rects start at `rail - panel` so
        // a half-open rail is the right-hand part of a whole one.
        let panel = if !visible || slide <= 0.0 { 0.0 } else if w < 900.0 { 210.0_f32.min(w * 0.3) } else { 280.0 };
        let ease = |t: f32| { let t = t.clamp(0.0, 1.0); t * t * (3.0 - 2.0 * t) };
        let rail = (panel * ease(slide)).round();
        let ox = rail - panel;
        if visible && cut {
            // Timeline Edit (12:2): the source rail, the canvas and the
            // inspector share the top; the transport and the lanes run the
            // full width underneath (compact lanes on short windows). The
            // rail keeps its focused inspector; its action row stays in
            // Sources, where Duplicate / Add files live.
            let lanes = if h >= CUT_FULL_MIN_H { CUT_TIMELINE_H } else { CUT_TIMELINE_COMPACT_H };
            // The inspector keeps its width and slides past the right edge;
            // `side_in` is the part still inside, which the canvas reclaims.
            let side_w = if w >= 1000.0 { CUT_SIDE_W } else { 0.0 };
            let side_in = (side_w * ease(side_slide)).round();
            let side = if side_in > 0.0 { side_w } else { 0.0 };
            let transport_y = h - STATUS_H - lanes - CUT_TOOLBAR_H;
            let top = (transport_y - CUT_HEADER_H).max(1.0);
            let (grip, inspector) = Self::detail(ox, panel, h, transport_y, detail_slide);
            let list_y = HEADER_H + 14.0;
            return Self {
                rail, panel, inspector, grip,
                list: RectPx { x: ox, y: list_y, w: panel, h: (grip.y - RAIL_FOOTER_H - list_y).max(SOURCE_H) },
                canvas: RectPx { x: rail, y: CUT_HEADER_H, w: (w - side_in - rail).max(1.0), h: top },
                side: RectPx { x: w - side_in, y: CUT_HEADER_H, w: side, h: top },
                transport: RectPx { x: 0.0, y: transport_y, w, h: CUT_TOOLBAR_H },
                timeline: RectPx { x: 0.0, y: transport_y + CUT_TOOLBAR_H, w, h: lanes },
            };
        }
        let header = if visible { HEADER_H + CONTEXT_H } else { 0.0 };
        let bottom = if visible { TRANSPORT_H + STATUS_H } else { 0.0 };
        let canvas = RectPx { x: rail, y: header, w: (w - rail).max(1.0), h: (h - header - bottom).max(1.0) };
        // Sources sidebar (278:584): the list starts 14px under the header
        // and the focused inspector is 216px. The app's own rows — the
        // footer line above the inspector and the Duplicate / Add files
        // buttons under it — are not in the design and take their own
        // strips, so nothing overlaps the inspector's path line.
        let (grip, inspector) = Self::detail(ox, panel, h, h - STATUS_H - RAIL_ACTIONS_H, detail_slide);
        let list_y = HEADER_H + 14.0;
        Self {
            rail, panel, canvas, inspector, grip, timeline: none, side: none,
            list: RectPx { x: ox, y: list_y, w: panel, h: (grip.y - RAIL_FOOTER_H - list_y).max(SOURCE_H) },
            transport: RectPx { x: rail, y: h - STATUS_H - TRANSPORT_H, w: w - rail, h: TRANSPORT_H },
        }
    }

    /// The grip strip and the visible part of the focused inspector above
    /// `bottom`: the inspector folds down behind what is under it as
    /// `detail_slide` goes to 0, the grip riding on its top edge.
    fn detail(ox: f32, panel: f32, h: f32, bottom: f32, detail_slide: f32) -> (RectPx, RectPx) {
        let none = RectPx { x: ox, y: bottom, w: panel, h: 0.0 };
        if panel <= 0.0 || h < 800.0 { return (none, none); }
        let ease = |t: f32| { let t = t.clamp(0.0, 1.0); t * t * (3.0 - 2.0 * t) };
        let shown = (RAIL_INSPECTOR_H * ease(detail_slide)).round();
        let inspector = RectPx { x: ox, y: bottom - shown, w: panel, h: shown };
        let grip = RectPx { x: ox, y: inspector.y - RAIL_GRIP_H, w: panel, h: RAIL_GRIP_H };
        (grip, inspector)
    }
}

#[derive(Clone, Copy)]
enum Action { Duplicate, OpenFiles, ClearSources, ToggleSources, ToggleInspector, ToggleDetail, Tool(usize), View(Mode), Brush(bool), Save, Export, Param(bool), Cut(CutAction) }
#[derive(Clone, Copy, PartialEq)]
enum CutAction { TogglePosters, FrameChanges(bool), TimeDisplay(bool), In, Out, Split, ToggleSplit, Apply, DeletePicked, Undo, Export, EncodeAll, Encode(f64), Snap(Snap), Play, Zoom(bool), Step(i32), Keyframe(i32), Chapter(i32) }
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
    items.push(Item::Text(TextItem { interface: true, align, valign: VAlign::Middle, max_width: Some(width.max(0.0)),
        ..TextItem::new(x, y, px, color, text) }));
}
fn hex_color(rgb: u32) -> [f32; 4] {
    [((rgb >> 16) & 255) as f32 / 255.0, ((rgb >> 8) & 255) as f32 / 255.0, (rgb & 255) as f32 / 255.0, 1.0]
}
fn clip_color(index: usize) -> [f32; 4] {
    const COLORS: [u32; 9] = [0x61b5ee, 0xe6ac69, 0xb89ae8, 0x6dc7aa, 0xe88891, 0xc9c970, 0x70c8d2, 0xdb9bc6, 0xa4b9d4];
    hex_color(COLORS[index % COLORS.len()])
}
/// The texture window that fills `r` edge to edge at the clip's own
/// proportions (CSS `object-fit: cover`), as the renderer's uv CORNERS
/// `[u0, v0, u1, v1]` — not an origin and size: handing it a size put v1
/// below v0 on tall clips and drew them upside down.
fn cover_uv(info: &crate::probe::VideoInfo, r: RectPx) -> [f32; 4] {
    let aspect = info.width.max(1) as f32 / info.height.max(1) as f32;
    let (uw, uh) = if aspect > r.w / r.h.max(1.0) { (r.w / r.h.max(1.0) / aspect, 1.0) } else { (1.0, aspect * r.h / r.w.max(1.0)) };
    let (u0, v0) = ((1.0 - uw) / 2.0, (1.0 - uh) / 2.0);
    [u0, v0, u0 + uw, v0 + uh]
}
fn intersect(a: RectPx, b: RectPx) -> RectPx {
    let (x0, y0) = (a.x.max(b.x), a.y.max(b.y));
    let (x1, y1) = ((a.x + a.w).min(b.x + b.w), (a.y + a.h).min(b.y + b.h));
    RectPx { x: x0, y: y0, w: (x1 - x0).max(0.0), h: (y1 - y0).max(0.0) }
}
/// Abner / Toolbar / Sidebar toggle (278:573): Finder's sidebar glyph — a
/// rounded frame with a narrow leading pane of three dots — on a neutral
/// #292929 plate while the panel is shown, bare while it is hidden. Rects
/// only: the renderer has no path primitive.
fn draw_sidebar_toggle(items: &mut Vec<Item>, r: RectPx, right: bool, shown: bool, hovered: bool) {
    if shown || hovered {
        items.push(Item::Rect(RectItem { radius: 5.0, ..RectItem::new(r, hex_color(if shown { 0x292929 } else { 0x1c1c1c })) }));
    }
    let ink = hex_color(if shown { 0xededec } else { 0x8f8f8f });
    let f = RectPx { x: r.x + 6.0, y: r.y + 6.0, w: 20.0, h: 16.0 };
    items.push(Item::Rect(RectItem { radius: 3.5, border_w: 1.5, border_color: ink, ..RectItem::new(f, [0.0; 4]) }));
    // The pane sits on the side the panel lives on (Side=Right mirrors it).
    let (bar, dots) = if right { (f.x + f.w - 8.0, f.x + f.w - 4.75) } else { (f.x + 6.5, f.x + 3.25) };
    items.push(Item::Rect(RectItem::new(RectPx { x: bar, y: f.y + 1.0, w: 1.5, h: f.h - 2.0 }, ink)));
    for k in 0..3 {
        items.push(Item::Rect(RectItem::new(RectPx { x: dots, y: f.y + 4.0 + k as f32 * 3.0, w: 1.5, h: 1.5 }, ink)));
    }
}
fn number_badge(items: &mut Vec<Item>, r: RectPx, index: usize) {
    let color = clip_color(index);
    let bg = [color[0] * 0.17 + 0.04, color[1] * 0.17 + 0.04, color[2] * 0.17 + 0.04, 1.0];
    items.push(Item::Rect(RectItem { radius: 5.0, border_w: 1.0,
        border_color: [color[0], color[1], color[2], 0.12], ..RectItem::new(r, bg) }));
    ui_label(items, r.x + r.w / 2.0, r.y + r.h / 2.0, 11.0, color, (index + 1).to_string(), Align::Center, r.w - 4.0);
}
/// Timeline Edit / Header (12:5): 44px shell with a blue selected tab.
const CUT_HEADER_H: f32 = 44.0;
const CUT_HEADER_BLUE: u32 = 0x0074e1;
const HEADER_H: f32 = CUT_HEADER_H;
const CONTEXT_H: f32 = 40.0;
/// Sources sidebar (278:584): 157px cards on a 12px gap, a 100px preview.
const SOURCE_H: f32 = 169.0;
const SOURCE_THUMB_H: f32 = 100.0;
/// The app's rows the design leaves out: the footer line above the
/// inspector and the Duplicate / Add files buttons under it.
const RAIL_FOOTER_H: f32 = 36.0;
/// The sidebar's slide, seconds (the design's "panels slide outward over 200 ms").
const RAIL_SLIDE_S: f32 = 0.2;
const RAIL_ACTIONS_H: f32 = 40.0;
/// The focused inspector (278:609) is 216px; the grip strip along its top
/// edge folds it away over `DETAIL_SLIDE_S` — quicker than the rail, it
/// moves less.
const RAIL_INSPECTOR_H: f32 = 216.0;
const RAIL_GRIP_H: f32 = 14.0;
const DETAIL_SLIDE_S: f32 = 0.15;
/// The grip as measured off the Figma: an 8px #151515 strip under a 1px
/// #060606 line, three 5×4 #2a2a2a discs outlined in near-black on a 6px
/// pitch.
const RAIL_GRIP_STRIP_H: f32 = 8.0;
const RAIL_GRIP_BG: u32 = 0x151515;
const RAIL_GRIP_LINE: u32 = 0x060606;
const RAIL_GRIP_DOT: (f32, f32) = (5.0, 4.0);
const RAIL_GRIP_PITCH: f32 = 6.0;
const RAIL_GRIP_DOT_FILL: u32 = 0x2a2a2a;
/// The focused card's ring, the idle card's grey edge (as the design's
/// card renders, #444444), and the keycap plate (white @ 0.5 in Figma;
/// linear blending wants less).
const SOURCE_RING: u32 = 0xff9d00;
const SOURCE_EDGE: u32 = 0x444444;
const SOURCE_EDGE_HOVER: u32 = 0x5c5c5c;
const SOURCE_KEYCAP: [f32; 4] = [1.0, 1.0, 1.0, 0.3];
/// Focused inspector (278:609): its ground, the dashed hairlines, the path.
const INSPECTOR_BG: u32 = 0x0f0f0f;
const INSPECTOR_DASH: u32 = 0x252525;
const INSPECTOR_PATH: u32 = 0x3694eb;
/// Sources card variables: surface, border, ink, secondary, subtle.
const SOURCE_SURFACE: u32 = 0x11151b;
const SOURCE_BORDER: u32 = 0x323b47;
const SOURCE_INK: u32 = 0xf0f3f7;
const SOURCE_SECONDARY: u32 = 0xb3bdc9;
const SOURCE_SUBTLE: u32 = 0x8d9aab;
const WORKSPACE_PANEL: [f32; 4] = [0.0196, 0.0196, 0.0235, 1.0];
const CANVAS_BG: [f32; 4] = [29.0 / 255.0, 27.0 / 255.0, 27.0 / 255.0, 1.0];
const WORKSPACE_TEXT: [f32; 4] = [209.0 / 255.0, 209.0 / 255.0, 209.0 / 255.0, 1.0];
const WORKSPACE_DIM: [f32; 4] = [0.57, 0.57, 0.57, 1.0];
const WORKSPACE_MUTED: [f32; 4] = [0.43, 0.43, 0.43, 1.0];
const WORKSPACE_RULE: [f32; 4] = [0.09, 0.09, 0.09, 1.0];
const CONTROL_BG: [f32; 4] = [26.0 / 255.0, 26.0 / 255.0, 26.0 / 255.0, 1.0];

#[cfg(test)]
mod tests {
    #[test]
    fn frame_detail_loads_real_images_and_selects_only_a_gop() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 1);
        app.playing = false;
        app.start_frame_detail(false);
        assert!(tick_until(&mut app, Duration::from_secs(10), |a| a.frame_detail() && !a.frame_detail_boot));
        let cut = app.cut.as_ref().unwrap();
        assert!((cut.pps * cut.duration / cut.frames.len() as f64 - 160.0).abs() < 1e-4);
        let before = cut.pps;
        app.cut_action(CutAction::Zoom(true));
        assert_eq!(app.cut.as_ref().unwrap().pps, before, "maximum detail is a hard ceiling");
        assert!(tick_until(&mut app, Duration::from_secs(20), |a| a.frame_tiles().iter().all(|(i,_,_,k,_)| a.cut_thumb.slot_of(a.frame_thumb_key(*i,*k)).is_some())));
        assert!(app.cut_thumb.pending.len() <= 2);
        let tile = app.frame_tiles()[2].4;
        let expected = app.cut.as_ref().unwrap().gop_at(app.cut_time_at(tile.x+tile.w/2.0)).unwrap();
        app.mouse_down(tile.x+tile.w/2.0,tile.y+tile.h/2.0);app.mouse_up();
        assert!(app.cut_gop);
        assert!((app.t-app.cut.as_ref().unwrap().frames[2]).abs()<1e-6,"clicking a card seeks that frame, not the next one");
        assert_eq!(app.cut_picks, vec![(expected.0,expected.1,false)]);
        assert!(expected.1 < app.cut.as_ref().unwrap().duration);
        app.key(Key::Backspace);
        assert!(app.cut.as_ref().unwrap().segments().iter().any(|s| s.cut && s.start==expected.0 && s.end==expected.1));
        app.cut_action(CutAction::Undo);
        let r=app.frame_mode_rects()[1];app.mouse_down(r.x+10.0,r.y+10.0);app.mouse_up();
        assert!(app.frame_changes);
        assert!(tick_until(&mut app, Duration::from_secs(20), |a| a.frame_tiles().iter().all(|(i,_,_,k,_)| a.cut_thumb.slot_of(a.frame_thumb_key(*i,*k)).is_some())));
        let decoded=app.cut_thumb.slot_of(-3).unwrap();
        let changes=app.cut_thumb.slot_of(-4).unwrap();
        assert_ne!(app.cut_thumb.cells[decoded].rgba,app.cut_thumb.cells[changes].rgba);
        for size in [(720.0,480.0),(1280.0,800.0)] {
            app.vp=size;
            let w=app.workspace();let lanes=app.cut_lanes();
            assert!(w.canvas.h>0.0 && w.transport.y>w.canvas.y);
            assert!((w.side.y+w.side.h-w.transport.y).abs()<0.01);
            assert!(lanes.keys.y+lanes.keys.h+39.0 <= w.timeline.y+w.timeline.h+0.01);
            let controls=app.controls();
            assert_eq!(controls.iter().filter(|c| matches!(c.action,Action::Cut(CutAction::FrameChanges(_)))).count(),2);
        }
    }


    #[test]
    fn encoding_regions_remain_visible_and_toolbar_fits() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 1);
        app.set_cut_default(true);
        assert!(tick_until(&mut app, Duration::from_secs(5), |a| !a.cut.as_ref().unwrap().scanning()));
        {
            let cut = app.cut.as_mut().unwrap();
            cut.snap = Snap::Frame; cut.split(cut.frames[10]); cut.pps = 1200.0;
        }
        let pending = app.cut.as_ref().unwrap().pending_gops().len();
        assert!(pending > 0);
        let video = app.cut_lanes().video;
        assert!(app.cut_encode_regions().iter().all(|(_,r)| r.y >= video.y + video.h));
        assert!(app.cut_thumb_rects().is_empty(), "frame cells replace the poster at close zoom");
        assert_eq!(app.cut.as_ref().unwrap().pending_gops().len(), pending);
        for width in [720.0, 900.0, 1280.0] {
            app.vp = (width, 800.0);
            let controls = app.controls();
            let all = controls.iter().find(|c| matches!(c.action, Action::Cut(CutAction::EncodeAll))).unwrap();
            assert!(all.r.x >= 0.0 && all.r.x + all.r.w <= width);
            if all.r.y >= HEADER_H { assert!(all.r.x >= app.cut_tools_x() + 5.0*38.0 && all.r.x + all.r.w < app.cut_clock_x()); }
            assert!(!app.cut_encode_regions().is_empty());
        }
    }

    #[test]
    fn frame_marker_detail_follows_zoom_and_key_flag() {
        let mut items = Vec::new();
        let r = RectPx { x: 0.0, y: 0.0, w: 9.0, h: 56.0 };
        draw_frame_marker(&mut items, r, 'B', false, 35);
        assert!(items.is_empty());
        for (width, labels) in [(10.0, 0), (31.0, 0), (32.0, 1), (79.0, 1), (80.0, 2)] {
            items.clear();
            draw_frame_marker(&mut items, RectPx { w: width, ..r }, 'B', false, 35);
            assert_eq!(items.iter().filter(|i| matches!(i, Item::Text(_))).count(), labels);
        }
        items.clear();
        draw_frame_marker(&mut items, RectPx { w: 112.0, ..r }, 'I', false, 0);
        let Item::Rect(i) = &items[0] else { panic!() };
        assert_eq!(i.border_color, hex_color(FRAME_I));
        items.clear();
        draw_frame_marker(&mut items, RectPx { w: 112.0, ..r }, 'I', true, 0);
        let Item::Rect(i) = &items[0] else { panic!() };
        assert_eq!(i.border_color, hex_color(FRAME_KEY));
    }

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
    fn inspector_directory_has_pointer_and_opens_the_focused_path() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 1);
        app.vp = (1280.0, 800.0);
        let inspector = app.workspace().inspector;
        let (x, y) = (inspector.x + 24.0, inspector.y + 195.0);
        app.cursor_moved(x, y);
        assert!(matches!(app.crop_cursor(), Some(CropCursor::Pointer)));
        app.mouse_down(x, y);
        assert!(app.take_cmds().iter().any(|cmd| matches!(cmd,
            Cmd::OpenDirectory(path) if Some(path.as_path()) == clip.parent())));
        app.set_sources_visible(false);
        app.cursor_moved(x, y);
        assert!(!matches!(app.crop_cursor(), Some(CropCursor::Pointer)));
    }

    #[test]
    fn native_workspace_selection_matches_header_for_every_transition() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 1);
        for from in 0..4 {
            for to in 0..4 {
                app.select_workspace_mode(from);
                app.select_workspace_mode(to);
                assert_eq!(app.workspace_mode(), to, "{from} -> {to}");
                let selected: Vec<_> = app.mode_selector().controls().into_iter()
                    .filter(|c| c.selected).collect();
                assert_eq!(selected.len(), 1);
                assert!(matches!(selected[0].action, Action::Tool(mode) if mode == to));
            }
        }
        app.select_workspace_mode(0);
        app.key(Key::Char('m'));
        assert_eq!(app.workspace_mode(), 1);
        app.key(Key::Escape);
        assert_eq!(app.workspace_mode(), 0);
        app.select_workspace_mode(99);
        assert_eq!(app.workspace_mode(), 0);
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
    fn source_checkboxes_do_not_change_view_and_clear_keeps_focus() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 3);
        app.vp = (1280.0, 800.0);
        let r = app.source_checkbox(1);
        app.mouse_down(r.x + 10.0, r.y + 10.0);
        assert_eq!(app.active, 0);
        assert!(app.videos[1].selected);
        app.key(Key::Char('3'));
        assert_eq!(app.active, 2);
        assert!(app.videos[1].selected);
        assert!(!app.videos[2].selected);
        app.key(Key::Char('x'));
        assert!(app.videos[2].selected);
        app.activate(Action::ClearSources);
        assert_eq!(app.active, 2);
        assert!(app.videos.iter().all(|v| !v.selected));
        app.key(Key::SelectSources);
        assert!(app.videos.iter().all(|v| v.selected));
        app.key(Key::SelectSources);
        assert!(app.videos.iter().all(|v| !v.selected));
        let desc = app.tick(0.0, (720.0, 480.0), 2.0);
        drop(desc);
        assert_eq!(app.source_first(), 2, "resize keeps the viewed source visible");
    }

    #[test]
    fn duplicate_sources_preserve_time_media_and_independent_selection() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 2);
        assert!(tick_until(&mut app, Duration::from_secs(3), |a| a.started && a.t > 0.1));
        app.playing = false;
        let t = app.t;
        let size = std::fs::metadata(&clip).unwrap().len();
        app.videos[0].selected = true;
        app.videos[1].selected = true;
        app.key(Key::DuplicateSources);
        assert_eq!(app.t, t);
        assert!(!app.playing);
        assert_eq!(app.videos.len(), 4);
        assert_eq!(app.active, 1);
        assert_eq!(app.source_file_count(), 1);
        assert_eq!(app.videos.iter().map(|v| v.selected).collect::<Vec<_>>(), [false, true, false, true]);
        assert_eq!(app.videos.iter().map(|v| v.linked_copy).collect::<Vec<_>>(), [false, true, false, true]);
        assert!(app.videos.iter().all(|v| v.info.path == clip));
        assert_eq!(std::fs::metadata(&clip).unwrap().len(), size);
        assert!(app.take_cmds().iter().any(|c| matches!(c, Cmd::VideosChanged)));
        assert!(tick_until(&mut app, Duration::from_secs(3), |a| a.videos.iter().all(|v| v.delivered && !v.pending)));
        for v in &app.videos { assert!((v.shown_pts - app.videos[0].shown_pts).abs() < 1.0 / app.fps); }
        app.key(Key::Close);
        assert_eq!(app.videos.len(), 3);
        assert!(clip.exists());
        assert!(!app.videos[1].selected);
        assert!(app.videos[2].selected);
        app.add_videos(vec![mk_video(&clip)], true);
        assert_eq!(app.videos.len(), 1);
        assert!(!app.videos[0].selected && !app.videos[0].linked_copy);
    }

    #[test]
    fn duplicate_viewed_source_while_playing_stays_frame_locked() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 1);
        assert!(tick_until(&mut app, Duration::from_secs(3), |a| a.started && a.t > 0.1));
        let t = app.t;
        app.duplicate_sources();
        assert!(app.playing);
        assert_eq!(app.t, t);
        assert_eq!(app.videos.len(), 2);
        assert!(tick_until(&mut app, Duration::from_secs(3), |a| a.videos.iter().all(|v| v.delivered)));
        assert!((app.videos[0].shown_pts - app.videos[1].shown_pts).abs() <= 1.0 / app.fps + 1e-6);
    }

    #[test]
    fn native_titlebar_excludes_controls_content_and_fullscreen() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 2);
        app.vp = (1280.0, 800.0);
        assert!(app.titlebar_background(600.0, 20.0));
        assert!(!app.titlebar_background(50.0, 20.0));
        assert!(!app.titlebar_background(100.0, 20.0));
        assert!(!app.titlebar_background(600.0, HEADER_H));
        let original = app.mode_selector().controls();
        for mode in [3, 2, 1, 0] {
            app.activate(Action::Tool(mode));
            let controls = app.mode_selector().controls();
            assert_eq!(controls.len(), 4);
            assert_eq!(controls.iter().filter(|c| c.selected).count(), 1);
            for (before, after) in original.iter().zip(&controls) {
                assert_eq!((before.r.x, before.r.y, before.r.w, before.r.h),
                    (after.r.x, after.r.y, after.r.w, after.r.h), "mode changes cannot move the shared selector");
                assert_eq!(after.selected, matches!(after.action, Action::Tool(i) if i == mode));
            }
        }
        app.set_cut_mode(true);
        for c in app.controls().iter().filter(|c| c.r.y < HEADER_H) {
            assert!(!app.titlebar_background(c.r.x + c.r.w / 2.0, c.r.y + c.r.h / 2.0));
        }
        app.fullscreen = true;
        assert!(!app.titlebar_background(600.0, 20.0));
        app.fullscreen = false;
        app.show_ui = false;
        assert!(!app.titlebar_background(600.0, 20.0));
    }

    /// The grip along the top edge of the rail's focused inspector
    /// folds the panel down behind the action row over DETAIL_SLIDE_S and
    /// back; the list takes the room, the grip stays reachable, and the
    /// directory row stops answering while the panel is away.
    #[test]
    fn inspector_grip_folds_the_detail_panel_and_the_list_takes_the_room() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 2);
        app.vp = (1280.0, 900.0);
        let open = app.workspace();
        assert_eq!((open.inspector.h, open.grip.h), (RAIL_INSPECTOR_H, RAIL_GRIP_H));
        assert_eq!(open.grip.y + open.grip.h, open.inspector.y);
        assert!(open.list.y + open.list.h + RAIL_FOOTER_H <= open.grip.y);
        assert!(app.controls().iter().any(|c| matches!(c.action, Action::ToggleDetail) && c.selected));
        app.cursor_moved(open.grip.x + 140.0, open.grip.y + 7.0);
        assert!(matches!(app.crop_cursor(), Some(CropCursor::Pointer)));
        app.mouse_down(open.grip.x + 140.0, open.grip.y + 7.0);
        app.mouse_up();
        assert!(!app.show_detail);
        let desc = app.tick(DETAIL_SLIDE_S / 2.0, (1280.0, 900.0), 2.0);
        assert!(desc.animating);
        let mid = app.workspace();
        assert!(mid.inspector.h > 0.0 && mid.inspector.h < RAIL_INSPECTOR_H, "{}", mid.inspector.h);
        assert!(mid.inspector.y > open.inspector.y && mid.list.h > open.list.h);
        assert!(!app.inspector_path_hit(mid.inspector.x + 24.0, mid.inspector.y + 195.0));
        app.tick(DETAIL_SLIDE_S, (1280.0, 900.0), 2.0);
        let shut = app.workspace();
        assert_eq!(shut.inspector.h, 0.0);
        assert_eq!(shut.grip.y + shut.grip.h, open.inspector.y + RAIL_INSPECTOR_H);
        assert_eq!(shut.list.h, open.list.h + RAIL_INSPECTOR_H);
        assert!(app.controls().iter().any(|c| matches!(c.action, Action::ToggleDetail) && !c.selected));
        // The grip rode down with the edge; a click there brings it back.
        app.mouse_down(shut.grip.x + 140.0, shut.grip.y + 7.0);
        app.mouse_up();
        app.tick(DETAIL_SLIDE_S, (1280.0, 900.0), 2.0);
        let back = app.workspace();
        assert_eq!((back.inspector.y, back.inspector.h, back.list.h), (open.inspector.y, RAIL_INSPECTOR_H, open.list.h));
        // Hiding the rail takes the grip with it.
        app.set_sources_visible(false);
        assert_eq!(app.workspace().grip.h, 0.0);
    }

    /// The header's sidebar toggle (Figma 278:573) hides the Sources rail:
    /// the canvas takes the width, the rail's own controls go with it, the
    /// shared selector stays put, and the state survives a mode switch.
    #[test]
    fn sidebar_toggle_hides_the_source_rail_and_survives_mode_switches() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 2);
        app.vp = (1280.0, 800.0);
        let tabs = app.header_tabs();
        assert_eq!(app.workspace().rail, 280.0);
        assert!(app.controls().iter().any(|c| matches!(c.action, Action::ToggleSources) && c.selected));
        let r = app.sidebar_toggle();
        assert!(r.x + r.w <= tabs.x, "the toggle sits before the selector");
        app.mouse_down(r.x + 16.0, r.y + 14.0);
        app.mouse_up();
        assert!(!app.show_sources);
        // The panel slides out over RAIL_SLIDE_S: half-way the layout has
        // given the canvas part of the width while the cards keep theirs
        // and sit partly off-screen; settled, the rail is gone.
        app.tick(RAIL_SLIDE_S / 2.0, (1280.0, 800.0), 2.0);
        let mid = app.workspace();
        assert!(mid.rail > 0.0 && mid.rail < 280.0, "{}", mid.rail);
        assert_eq!((mid.panel, mid.canvas.x, mid.list.w), (280.0, mid.rail, 280.0));
        assert_eq!(app.source_row(0).w, 256.0);
        assert!(app.source_row(0).x < 12.0);
        let desc = app.tick(RAIL_SLIDE_S, (1280.0, 800.0), 2.0);
        assert!(!desc.animating || app.playing);
        let l = app.workspace();
        assert_eq!((l.rail, l.canvas.x, l.canvas.w, l.list.w, l.inspector.h), (0.0, 0.0, 1280.0, 0.0, 0.0));
        assert!(app.controls().iter().all(|c| !matches!(c.action, Action::Duplicate | Action::OpenFiles)));
        assert!(app.controls().iter().any(|c| matches!(c.action, Action::ToggleSources) && !c.selected));
        let t = app.header_tabs();
        assert_eq!((t.x, t.y, t.w, t.h), (tabs.x, tabs.y, tabs.w, tabs.h));
        // Clicking where a card used to be reaches the canvas, not a clip.
        app.mouse_down(60.0, HEADER_H + 80.0);
        app.mouse_up();
        assert_eq!(app.active, 0);
        app.key(Key::Char('m'));
        assert!(app.mask_mode);
        assert_eq!(app.workspace().rail, 0.0);
        app.mouse_down(r.x + 16.0, r.y + 14.0);
        app.mouse_up();
        assert!(app.show_sources && app.mask_mode);
        app.tick(RAIL_SLIDE_S, (1280.0, 800.0), 2.0);
        assert_eq!(app.workspace().rail, 280.0);
        // Timeline mode keeps the rail beside the video and a click there
        // re-cuts the chosen clip; the toggle hides it there too.
        app.key(Key::Char('m'));
        app.vp = (1280.0, 1100.0);
        app.set_cut_mode(true);
        assert!(app.cut_mode);
        let l = app.workspace();
        assert_eq!((l.rail, l.canvas.x, l.transport.x, l.timeline.x), (280.0, 280.0, 0.0, 0.0));
        assert!(l.canvas.x + l.canvas.w <= l.side.x);
        let second = app.source_row(1);
        assert!(second.y + second.h <= l.list.y + l.list.h, "two cards fit at this height");
        app.mouse_down(second.x + 60.0, second.y + 25.0);
        app.mouse_up();
        assert_eq!(app.active, 1);
        assert!(app.cut_mode && app.cut_video == 1);
        app.mouse_down(r.x + 16.0, r.y + 14.0);
        app.mouse_up();
        app.tick(RAIL_SLIDE_S, (1280.0, 1100.0), 2.0);
        let l = app.workspace();
        assert!(!app.show_sources && app.cut_mode);
        assert_eq!((l.rail, l.canvas.x), (0.0, 0.0));
        app.mouse_down(r.x + 16.0, r.y + 14.0);
        app.mouse_up();
        app.tick(RAIL_SLIDE_S, (1280.0, 1100.0), 2.0);
        // The right toggle slides the timeline's inspector off the right
        // edge, independently of the rail, and the canvas takes the width.
        let t = app.inspector_toggle();
        assert!(app.controls().iter().any(|c| matches!(c.action, Action::ToggleInspector) && c.selected));
        assert!(app.controls().iter().all(|c| !matches!(c.action, Action::Cut(CutAction::Export)) || c.r.x + c.r.w <= t.x));
        let before = app.workspace();
        assert_eq!((before.side.x, before.side.w), (1280.0 - CUT_SIDE_W, CUT_SIDE_W));
        app.mouse_down(t.x + 16.0, t.y + 14.0);
        app.mouse_up();
        app.tick(RAIL_SLIDE_S / 2.0, (1280.0, 1100.0), 2.0);
        let mid = app.workspace();
        assert!(mid.side.x > before.side.x && mid.side.x < 1280.0 && mid.side.w == CUT_SIDE_W);
        assert_eq!(mid.canvas.x + mid.canvas.w, mid.side.x);
        app.tick(RAIL_SLIDE_S, (1280.0, 1100.0), 2.0);
        let l = app.workspace();
        assert!(!app.show_inspector && app.show_sources);
        assert_eq!((l.side.w, l.canvas.x + l.canvas.w, l.rail), (0.0, 1280.0, 280.0));
        app.mouse_down(t.x + 16.0, t.y + 14.0);
        app.mouse_up();
        app.tick(RAIL_SLIDE_S, (1280.0, 1100.0), 2.0);
        assert_eq!(app.workspace().side.x, before.side.x);
        app.set_cut_mode(false);
        let card = app.source_row(0);
        assert_eq!((card.y, card.h), (HEADER_H + 14.0, 157.0));
        // The preview is a cover crop in uv CORNERS: never mirrored, never
        // stretched, centred on the picture.
        let wide = crate::probe::VideoInfo { width: 2880, height: 720, ..app.videos[0].info.clone() };
        let tall = crate::probe::VideoInfo { width: 720, height: 1280, ..app.videos[0].info.clone() };
        let r = RectPx { x: 0.0, y: 0.0, w: 252.0, h: 106.0 };
        for info in [&wide, &tall] {
            let [u0, v0, u1, v1] = cover_uv(info, r);
            assert!(u1 > u0 && v1 > v0, "{:?}", (u0, v0, u1, v1));
            assert!((u0 + u1 - 1.0).abs() < 1e-6 && (v0 + v1 - 1.0).abs() < 1e-6);
            let shown = (u1 - u0) * info.width as f32 / ((v1 - v0) * info.height as f32);
            assert!((shown - r.w / r.h).abs() < 1e-3, "crop keeps the box's proportion: {shown}");
        }
        assert_eq!(cover_uv(&wide, r)[1], 0.0, "a wide clip gives up width, not height");
        assert_eq!(cover_uv(&tall, r)[0], 0.0, "a tall clip gives up height, not width");
        assert_eq!(app.source_row(1).y - card.y, SOURCE_H);
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
        app.key(Key::Enter);
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
        app.key(Key::Tab);
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
        app.key(Key::Tab);
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
            crate::config::Config::default().frame_cache_bytes(),
        )
        .expect("spawn");
        Video { selected: false, linked_copy: false, info, player, shown_pts: 0.0, delivered: false, pending: false, seeking: false, seek_sent: std::time::Instant::now(), last_frame: None }
    }

    /// A playing app: most tests drive the clock, so they opt out of the
    /// paused-on-open default (`opened_clips_sit_paused_on_their_first_frame`
    /// covers that).
    fn mk_app(clip: &PathBuf, n: usize) -> App {
        let mut cfg = crate::config::Config::default();
        cfg.playback.start_paused = false;
        App::new((0..n).map(|_| mk_video(clip)).collect(), &cfg)
    }

    /// Opening files never starts playback: the clip shows its first frame
    /// and the clock holds at 0 until Space — at startup and on a drop.
    #[test]
    fn opened_clips_sit_paused_on_their_first_frame() {
        let Some(clip) = test_clip() else { return };
        let cfg = crate::config::Config::default();
        let mut app = App::new(vec![mk_video(&clip)], &cfg);
        assert!(!app.playing, "startup clips must open paused");
        assert!(tick_until(&mut app, Duration::from_secs(5), |a| a.videos[0].delivered));
        for _ in 0..30 {
            app.tick(0.016, (1280.0, 800.0), 2.0);
        }
        assert_eq!(app.t, 0.0, "the clock advanced while paused");

        app.playing = true;
        assert!(tick_until(&mut app, Duration::from_secs(5), |a| a.t > 0.1));
        app.add_videos(vec![mk_video(&clip)], false);
        assert!(!app.playing, "a dropped clip must not resume playback");
        assert!(tick_until(&mut app, Duration::from_secs(5), |a| a.started));
        assert_eq!(app.t, 0.0);
    }

    /// Tick until a condition holds (real decode runs behind this).
    fn tick_until(app: &mut App, within: Duration, cond: impl Fn(&App) -> bool) -> bool {
        let deadline = Instant::now() + within;
        while Instant::now() < deadline {
            let _ = app.tick(0.010, (1280.0, 800.0), 2.0);
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

        // Chapters remain in the inspector; the transport has five controls.
        assert_eq!(app.controls().iter().filter(|c| matches!(c.action,
            Action::Cut(CutAction::Play | CutAction::Step(_) | CutAction::Keyframe(_)))).count(), 5);
        let original_vp = app.vp;
        for width in [720.0, 900.0, 1440.0] {
            app.vp = (width, 800.0);
            let controls: Vec<_> = app.controls().into_iter().filter(|c| matches!(c.action,
                Action::Cut(a) if a != CutAction::Export)).collect();
            for (i, c) in controls.iter().enumerate() {
                assert!(c.r.x >= 0.0 && c.r.x + c.r.w <= width, "toolbar overflow: {}", c.label);
                for other in &controls[i + 1..] {
                    assert!(c.r.x + c.r.w <= other.r.x || other.r.x + other.r.w <= c.r.x
                        || c.r.y + c.r.h <= other.r.y || other.r.y + other.r.h <= c.r.y,
                        "overlapping toolbar controls: {} / {}", c.label, other.label);
                }
            }
            assert!(app.cut_tools_x() < app.cut_clock_x());
        }
        app.vp = original_vp;
        let press = |app: &mut App, want: CutAction| {
            let r = app.controls().into_iter().find(|c| matches!(c.action, Action::Cut(a) if a == want)).expect("button").r;
            app.mouse_down(r.x + r.w / 2.0, r.y + r.h / 2.0);
            app.mouse_up();
        };
        app.seek_all(0.2, true);
        app.key(Key::Char('}'));
        assert!((app.t - 1.5).abs() < 0.05, "next chapter, t = {}", app.t);
        app.seek_all(1.6, true);
        app.key(Key::Char('{'));
        assert!(app.t < 0.05, "just inside chapter two, back goes to chapter one, t = {}", app.t);
        press(&mut app, CutAction::Keyframe(1));
        assert!((app.t - 1.0).abs() < 0.05, "next keyframe, t = {}", app.t);
        press(&mut app, CutAction::Keyframe(-1));
        assert!(app.t < 0.05, "previous keyframe, t = {}", app.t);
        assert!(!app.playing);

        assert_eq!(app.controls().iter().filter(|c| matches!(c.action,
            Action::Cut(CutAction::Split | CutAction::ToggleSplit))).count(), 1,
            "the split icon replaces the scissors rather than adding a second button");
        // The toolbar arms the tool; clicking uses its preview and shares undo.
        app.seek_all(1.0, true);
        press(&mut app, CutAction::ToggleSplit);
        assert!(app.split_tool);
        assert!(app.cut.as_ref().unwrap().splits().is_empty());
        let lane = app.cut_lanes().video;
        let x = lane.x + ((1.0 - app.cut.as_ref().unwrap().t0) * app.cut.as_ref().unwrap().pps) as f32;
        app.mouse_down(x, lane.y + lane.h / 2.0);
        app.mouse_up();
        assert_eq!(app.cut.as_ref().unwrap().splits(), &[1.0]);
        press(&mut app, CutAction::ToggleSplit);
        assert!(!app.split_tool);
        app.cut_action(CutAction::Undo);
        assert!(app.cut.as_ref().unwrap().splits().is_empty());

        // The time display changes representation without seeking or playing.
        let time = app.t;
        press(&mut app, CutAction::TimeDisplay(true));
        assert!(app.cut_frames);
        assert_eq!(app.t, time);
        assert!(!app.playing);
        app.key(Key::Char('`'));
        assert!(!app.cut_frames);

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
        // Each card fetches its chapter's first frame into the poster atlas.
        let keys: Vec<i64> = app.cut.as_ref().unwrap().chapters.iter().map(|c| (c.start * 1000.0).round() as i64).collect();
        assert!(tick_until(&mut app, Duration::from_secs(15), |a| keys.iter().all(|k| a.cut_thumb.slot_of(*k).is_some())),
            "every chapter card gets its frame");
        app.mouse_down(side.list_btn.x + 4.0, side.list_btn.y + 4.0);
        app.mouse_up();
        assert!(!app.cut_thumbs);

        // The streams tab replaces the chapter rows.
        let side = app.cut_side();
        app.mouse_down(side.tab[3].x + 4.0, side.tab[3].y + 4.0);
        app.mouse_up();
        assert_eq!(app.cut_tab, 1);
        assert!(app.cut_chapter_rows().is_empty());
        app.cursor = (side.body.x + 20.0, side.body.y + 20.0);
        let timeline_scale = app.cut.as_ref().unwrap().pps;
        app.scroll(0.0, -10000.0);
        assert_eq!(app.info_scroll, app.info_scroll_max());
        assert_eq!(app.cut.as_ref().unwrap().pps, timeline_scale);
        app.scroll(0.0, 10000.0);
        assert_eq!(app.info_scroll, 0.0);
        for tab in [2, 3] {
            app.mouse_down(side.tab[tab - 1].x + 4.0, side.tab[tab - 1].y + 4.0);
            app.mouse_up();
            assert_eq!(app.cut_tab, tab);
            assert!(app.cut_chapter_rows().is_empty());
        }
        assert!(!app.cut_detail_rows().is_empty(), "segments tab contains the kept range");
        let side = app.cut_side();
        app.mouse_down(side.tab[0].x + 4.0, side.tab[0].y + 4.0);
        app.mouse_up();
        assert_eq!(app.cut_tab, 0);

        // A click on a picture clip picks it (the outline); elsewhere lets go; Esc clears.
        let v = app.cut_lanes().video;
        app.mouse_down(v.x + v.w * 0.1, v.y + 10.0);
        app.mouse_up();
        assert_eq!(app.cut_picks, vec![(0.0, 4.0, false)]);
        let r = app.cut_lanes().ruler;
        app.mouse_down(v.x + v.w * 0.1, r.y + 4.0);
        app.mouse_up();
        assert!(app.cut_picks.is_empty());
        app.mouse_down(v.x + v.w * 0.1, v.y + 10.0);
        app.mouse_up();
        assert!(!app.cut_picks.is_empty());
        app.key(Key::Escape);
        assert!(app.cut_picks.is_empty() && app.cut_mode, "Esc lets go of the pick before it leaves Cut mode");

        // Backspace removes the picked piece, clears the pick, and is undoable.
        app.mouse_down(v.x + v.w * 0.1, v.y + 10.0);
        app.mouse_up();
        app.key(Key::Backspace);
        assert_eq!(app.cut.as_ref().unwrap().cuts(), &[(0.0, 4.0)]);
        assert!(app.cut_picks.is_empty());
        app.key(Key::Backspace);
        assert_eq!(app.cut.as_ref().unwrap().cuts(), &[(0.0, 4.0)], "repeat must not restore a deleted piece");
        app.key(Key::Undo);
        assert!(app.cut.as_ref().unwrap().cuts().is_empty());

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
        assert_eq!(app.cut_restore, Some((1.0, 2.0)));
        assert_eq!(app.cut.as_ref().unwrap().selection(), None, "blank clip clicks do not set in/out");
        app.cut.as_mut().unwrap().in_req = Some(2.5);
        app.cut.as_mut().unwrap().out_req = Some(3.5);
        app.key(Key::Char('x'));
        assert!(app.cut.as_ref().unwrap().cuts().is_empty());
        assert!(app.cut_restore.is_none());
        assert_eq!(app.cut.as_ref().unwrap().in_req, Some(2.5));
        assert_eq!(app.cut.as_ref().unwrap().out_req, Some(3.5));
    }

    /// U undoes like ⌘Z, and a two-finger swipe over the timeline zooms about the pointer
    /// (up in, down out) while a sideways swipe pans.
    /// Seeks sent while one is still landing are held back and coalesced:
    /// the clock takes the newest target at once, the players hear about
    /// it once the earlier seek has delivered, and a scrub release lands
    /// the exact frame.
    #[test]
    fn seeks_coalesce_while_one_is_in_flight() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 2);
        let vp = (1280.0, 720.0);
        app.playing = false;
        app.seek_all(1.0, true);
        assert!(app.videos.iter().all(|v| v.seeking));
        app.seek_all(1.4, true);
        app.seek_all(1.8, true);
        assert_eq!(app.queued_seek, Some((1.8, true)), "newest target waits");
        assert_eq!(app.t, 1.8, "the clock moves at once");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while app.queued_seek.is_some() && std::time::Instant::now() < deadline {
            let frame = app.tick(0.016, vp, 1.0);
            assert!(frame.uploads.is_empty(), "superseded seek must not flash an intermediate frame");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(app.queued_seek.is_none(), "the queued seek dispatches once the first lands");
        while app.seek_in_flight() && std::time::Instant::now() < deadline {
            app.tick(0.016, vp, 1.0);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        for v in &app.videos {
            assert!((v.shown_pts - 1.8).abs() < 0.05, "stream shows {:.3}, wanted 1.8", v.shown_pts);
        }

        // A seek-bar press is an exact seek on the frame grid; the clock
        // stays where the pointer put it, and every stream lands on it.
        app.show_ui = true;
        app.tick(0.016, vp, 1.0);
        let s = app.seek_rect(vp);
        app.mouse_down(s.x + s.w * 0.5, s.y + s.h / 2.0);
        assert!(app.scrubbing);
        let want = app.quantize(0.5 * (app.wrap - 0.05));
        assert!((app.t - want).abs() < 1e-9, "scrub target is on the frame grid: {} vs {want}", app.t);
        assert!(app.queued_seek.is_none() && app.videos.iter().all(|v| v.seeking), "press: one seek, sent at once");
        app.mouse_up();
        assert!(app.queued_seek.is_none(), "release sends nothing new");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while (app.queued_seek.is_some() || app.seek_in_flight()) && std::time::Instant::now() < deadline {
            app.tick(0.016, vp, 1.0);
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!((app.t - want).abs() < 1e-9, "the decoder never moves a pointer-owned clock");
        for v in &app.videos {
            assert!((v.shown_pts - want).abs() < 1.0 / 30.0, "every stream shows the requested frame: {} vs {want}", v.shown_pts);
        }
    }

    /// A lone clip opens on the timeline; a second one drops back to Sources.
    #[test]
    fn a_lone_clip_opens_in_timeline_mode() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 1);
        assert!(!app.cut_mode, "tests start in Sources");
        app.set_cut_default(true);
        assert!(app.cut_mode, "one file should open on the timeline");
        app.add_videos(vec![mk_video(&clip)], false);
        assert!(!app.cut_mode, "two files compare in Sources");
        // Replacing with a single file goes back to the timeline.
        app.add_videos(vec![mk_video(&clip)], true);
        assert!(app.cut_mode);
        // --mask style: the mask tool takes it back.
        app.key(Key::Char('m'));
        assert!(app.mask_mode && !app.cut_mode);
    }

    /// A sound clip is picked (and outlined) like a picture clip, in its own row.
    #[test]
    fn a_sound_clip_can_be_picked_too() {
        let Some(_) = test_clip() else { return };
        let dir = std::env::temp_dir().join("abner_app_test");
        let clip = dir.join("with_audio.mp4");
        if !clip.exists() {
            let ok = Command::new("ffmpeg")
                .args(["-y", "-v", "error", "-f", "lavfi", "-i", "testsrc2=duration=4:size=320x180:rate=30"])
                .args(["-f", "lavfi", "-i", "sine=frequency=440:duration=4"])
                .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "30", "-c:a", "aac"])
                .arg(&clip).status().map(|s| s.success()).unwrap_or(false);
            assert!(ok, "failed to generate the clip with audio");
        }
        let mut app = mk_app(&clip, 1);
        assert!(tick_until(&mut app, Duration::from_secs(5), |a| a.started));
        app.key(Key::Char('t'));
        assert!(tick_until(&mut app, Duration::from_secs(10), |a| a.cut.as_ref().is_some_and(|c| !c.scanning() && !c.reading_wave() && !c.wave.is_empty())));
        app.key(Key::Space);
        let l = app.cut_lanes();
        let (v, a) = (l.video, l.audio.expect("a sound lane"));
        app.mouse_down(v.x + v.w * 0.4, a.y + a.h / 2.0);
        app.mouse_up();
        assert_eq!(app.cut_picks, vec![(0.0, 4.0, true)], "the sound clip is picked");
        app.mouse_down(v.x + v.w * 0.4, v.y + v.h / 2.0);
        app.mouse_up();
        assert_eq!(app.cut_picks, vec![(0.0, 4.0, false)], "and the picture clip replaces it");
        // A selected piece scrolled under the gutter keeps its outline clipped.
        app.cut.as_mut().unwrap().t0 = 1.0;
        for audio in [false, true] {
            app.cut_picks = vec![(0.0, 4.0, audio)];
            let mut items = Vec::new();
            app.build_cut_timeline(&mut items);
            let mut clip = None;
            let mut saw_outline = false;
            for item in &items {
                match item {
                    Item::Clip(r) => clip = *r,
                    Item::Rect(r) if r.border_w == 2.0 && r.border_color == hex_color(0xffffff) => {
                        let bounds = clip.expect("selection must remain clipped");
                        assert_eq!(bounds.x, v.x);
                        assert_eq!(bounds.w, v.w);
                        let lane = if audio { app.cut_lanes().audio.unwrap() } else { v };
                        assert_eq!(r.r.y, lane.y, "selection stays inside the clip");
                        assert_eq!(r.r.h, lane.h);
                        assert_eq!(r.radius, 7.0);
                        saw_outline = true;
                    }
                    _ => {}
                }
            }
            assert!(saw_outline);
            assert!(matches!(items.last(), Some(Item::Clip(None))));
        }
    }

    #[test]
    fn cut_thumbnail_posters_load_and_the_hovered_one_plays() {
        let Some(source) = test_clip() else { return };
        // A longer clip stays below individual-frame detail at fit-to-window.
        let clip = source.with_file_name("poster-hover.mp4");
        assert!(Command::new("ffmpeg").args(["-v","error","-y","-stream_loop","1","-i"]).arg(&source)
            .args(["-c","copy"]).arg(&clip).status().unwrap().success());
        let mut app = mk_app(&clip, 1);
        assert!(tick_until(&mut app, Duration::from_secs(5), |a| a.started));
        app.key(Key::Char('t'));
        assert!(tick_until(&mut app, Duration::from_secs(10), |a| a.cut.as_ref().is_some_and(|c| !c.scanning())));
        let rects = app.cut_thumb_rects();
        assert_eq!(rects.len(), 1, "one kept clip, one thumbnail");
        let (key, _, r) = rects[0].clone();
        let v = app.cut_lanes().video;
        assert!(r.x >= v.x && (r.w / r.h - 16.0 / 9.0).abs() < 0.05);
        assert_eq!(r.y, v.y + 1.0);
        assert_eq!(r.h, v.h - 2.0, "poster fills the clip height inside its border");
        let old_pps = app.cut.as_ref().unwrap().pps;
        app.cut.as_mut().unwrap().pps = 8.0;
        let narrow = app.cut_thumb_rects();
        assert_eq!(narrow.len(), 1, "short clips retain a cropped poster");
        assert!(narrow[0].2.w < r.w);
        app.cut.as_mut().unwrap().pps = old_pps;
        assert!(tick_until(&mut app, Duration::from_secs(10), |a| a.cut_thumb.slot_of(key).is_some()), "the poster arrives");
        assert!(app.cut_thumb.play.is_none(), "nothing plays until the pointer is on it");

        app.cursor_moved(r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert!(tick_until(&mut app, Duration::from_secs(10), |a| a.cut_thumb.play.is_some()));
        assert_eq!(app.cut_thumb.hover, Some(key));
        assert!(tick_until(&mut app, Duration::from_secs(10), |a| a.cut_thumb.play.as_ref().is_some_and(|p| p.t > p.start + 0.2)), "its own clock advances");

        let hover_time = app.cut_thumb.play.as_ref().unwrap().t;
        app.mouse_down(app.cursor.0, app.cursor.1);
        app.tick(0.010, (1280.0, 800.0), 2.0);
        assert!(app.cut_thumb.play.as_ref().is_some_and(|p| p.t >= hover_time), "pressing preserves the hover decoder");
        app.mouse_up();
        app.cursor_moved(r.x + r.w / 2.0, app.cut_lanes().ruler.y + 4.0);
        app.tick(0.010, (1280.0, 800.0), 2.0);
        assert!(app.cut_thumb.play.is_none() && app.cut_thumb.hover.is_none(), "leaving stops it");
    }

    #[test]
    fn fit_cell_crops_and_pads_to_the_atlas_cell() {
        let (cw, ch) = (recent::THUMB_W, recent::THUMB_H);
        let tall = vec![200u8; (cw * (ch + 40) * 4) as usize];
        let cell = fit_cell(&tall, cw, ch + 40);
        assert_eq!(cell.len(), (cw * ch * 4) as usize);
        assert_eq!(cell[0], 200);
        let short = vec![200u8; (cw * (ch - 40) * 4) as usize];
        let cell = fit_cell(&short, cw, ch - 40);
        assert_eq!((cell[0], cell[3]), (0, 255), "padding is opaque black");
        assert_eq!(cell[(21 * cw * 4) as usize], 200, "the picture sits in the middle");
    }

    #[test]
    fn split_guide_previews_pixels_without_moving_clock_and_restores_on_leave() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 1);
        app.set_cut_default(true);
        app.playing = false;
        assert!(tick_until(&mut app, Duration::from_secs(5), |a| !a.cut.as_ref().unwrap().scanning()));
        let pixels = |app: &mut App| {
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                let desc = app.tick(0.0, (1280.0, 800.0), 2.0);
                if let Some(upload) = desc.uploads.into_iter().last() { break upload.buf; }
                assert!(Instant::now() < deadline, "frame was not delivered");
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        app.seek_all(1.0, true);
        let expected = pixels(&mut app);
        app.seek_all(0.0, true);
        let original = pixels(&mut app);
        assert_ne!(expected, original);
        let lane = app.cut_lanes().video;
        let x = lane.x + ((1.0 - app.cut.as_ref().unwrap().t0) * app.cut.as_ref().unwrap().pps) as f32;
        app.split_tool = true;
        app.cursor_moved(x, lane.y + lane.h / 2.0);
        assert_eq!(app.cut_razor(), Some(1.0));
        assert_eq!(pixels(&mut app), expected, "canvas receives the split frame");
        assert_eq!(app.t, 0.0, "preview leaves the playhead alone");
        assert!(!app.playing);
        // Change target twice before delivery: only the newest seek may appear.
        app.cut.as_mut().unwrap().snap = Snap::Frame;
        app.cursor_moved(x + 10.0, lane.y + 20.0);
        app.update_split_preview();
        app.cursor_moved(x, lane.y + 20.0);
        app.update_split_preview();
        assert_eq!(pixels(&mut app), expected);
        app.cursor_left();
        assert_eq!(pixels(&mut app), original, "leaving restores the playhead frame");
        assert!(app.split_preview.is_none());
        assert_eq!(app.t, 0.0);
        app.cursor_moved(x, lane.y + 20.0);
        assert_eq!(pixels(&mut app), expected);
        app.split_tool = false;
        assert_eq!(pixels(&mut app), original, "turning the tool off restores playback");
        assert!(app.split_preview.is_none());
    }

    #[test]
    fn movie_icon_toggles_clip_posters_without_seeking() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 1);
        app.set_cut_default(true);
        app.playing = false;
        app.tick(0.0, (1280.0, 800.0), 2.0);
        app.cut.as_mut().unwrap().pps = 100.0;
        assert!(!app.cut_thumb_rects().is_empty());
        let control = app.controls().into_iter().find(|c| matches!(c.action, Action::Cut(CutAction::TogglePosters))).unwrap();
        let t = app.t;
        app.mouse_down(control.r.x + 12.0, control.r.y + 12.0);
        app.mouse_up();
        app.tick(0.0, (1280.0, 800.0), 2.0);
        assert!(app.cut_thumb_rects().is_empty());
        assert!(app.cut_thumb.play.is_none());
        assert_eq!(app.t, t);
        assert!(app.cut_drag.is_none());
        app.mouse_down(control.r.x + 12.0, control.r.y + 12.0);
        app.mouse_up();
        assert!(!app.cut_thumb_rects().is_empty());
        assert_eq!(app.t, t);
    }

    #[test]
    fn hover_playhead_matches_click_and_hides_for_other_actions() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 1);
        app.set_cut_default(true);
        assert!(tick_until(&mut app, Duration::from_secs(5), |a| !a.cut.as_ref().unwrap().scanning()));
        app.playing = false;
        for detail in [false, true] {
            if detail { app.cut.as_mut().unwrap().pps = app.fps * 100.0; }
            let lane = app.cut_lanes().ruler;
            let x = lane.x + 143.0;
            let y = lane.y + 4.0;
            app.cursor_moved(x, y);
            let before = app.t;
            let target = app.cut_hover_playhead().expect("ruler previews a seek");
            assert_eq!(app.t, before, "hover does not seek");
            app.mouse_down(x, y);
            assert!((app.t - target).abs() < 1e-9);
            assert!(app.cut_hover_playhead().is_none(), "hidden while dragging");
            app.mouse_up();
        }
        let lane = app.cut_lanes().video;
        app.split_tool = true;
        app.cursor_moved(lane.x + 143.0, lane.y + 20.0);
        assert!(app.cut_hover_playhead().is_none(), "split tool does not seek");
        let ruler = app.cut_lanes().ruler;
        app.cursor_moved(ruler.x + 143.0, ruler.y + 4.0);
        assert!(app.cut_hover_playhead().is_none(), "split tool hides preview outside clips too");
        app.split_tool = false;
        assert!(app.cut_hover_playhead().is_some(), "turning split off restores the preview");
        app.cursor_moved(lane.x - 20.0, lane.y);
        assert!(app.cut_hover_playhead().is_none(), "gutter does not seek");
        app.cursor_inside = false;
        assert!(app.cut_hover_playhead().is_none());
    }

    #[test]
    fn split_tool_preview_and_click_use_nearest_key_or_next_frame() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 1);
        app.key(Key::Char('t'));
        app.tick(0.01, (1280.0, 800.0), 2.0);
        app.playing = false;
        app.cut.as_mut().unwrap().set_keys(vec![0.0, 1.0, 2.0, 3.0]);
        let lane = app.cut_lanes().video;
        let at = |app: &App, t: f64| lane.x + ((t - app.cut.as_ref().unwrap().t0) * app.cut.as_ref().unwrap().pps) as f32;
        app.cursor_moved(at(&app, 1.7), lane.y + lane.h / 2.0);
        assert!(app.cut_razor().is_none(), "no preview with the tool off");
        app.cut_action(CutAction::ToggleSplit);
        assert_eq!(app.cut_razor(), Some(2.0), "nearest key, not the previous key");
        let time = app.t;
        app.mouse_down(app.cursor.0, app.cursor.1);
        app.mouse_up();
        assert_eq!(app.cut.as_ref().unwrap().splits(), &[2.0]);
        assert_eq!(app.t, time, "splitting does not scrub");
        assert!(app.split_tool, "tool remains armed for repeated cuts");
        app.cut_action(CutAction::Snap(Snap::Frame));
        app.cursor_moved(at(&app, 1.213), lane.y + lane.h / 2.0);
        let expected = (1.213 * app.fps).ceil() / app.fps;
        assert!((app.cut_razor().unwrap() - expected).abs() < 1e-6);
        app.mouse_down(app.cursor.0, app.cursor.1);
        assert!(app.cut.as_ref().unwrap().splits().iter().any(|t| (*t - expected).abs() < 1e-6));
        app.key(Key::Escape);
        assert!(!app.split_tool && app.cut_mode);
        assert!(app.cut_razor().is_none());
    }

    #[test]
    fn encode_buttons_render_gops_and_replace_the_working_source() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 1);
        app.key(Key::Char('t')); app.playing = false;
        assert!(tick_until(&mut app, Duration::from_secs(5), |a| {
            let c = a.cut.as_ref().unwrap();
            !c.scanning() && !c.frames.is_empty() && c.frame_info.len() == c.frames.len()
        }));
        let at = app.cut.as_ref().unwrap().frames[10];
        let cut = app.cut.as_mut().unwrap(); cut.snap = Snap::Frame; cut.split(at);
        assert_eq!(cut.pending_gops()[0].encode_start, at);
        let regions = app.cut_encode_regions();
        assert_eq!(regions.len(), 1, "I/P prefix has no purple encoding region");
        let (_, region) = regions.into_iter().find(|(_, r)| r.w >= 54.0).unwrap();
        let lane = app.cut_lanes().video;
        let c = app.cut.as_ref().unwrap();
        assert!(region.x >= lane.x + ((at-c.t0)*c.pps) as f32);
        let button = App::encode_button(region);
        app.mouse_down(button.x+button.w/2.0, button.y+button.h/2.0); app.mouse_up();
        assert!(app.cut.as_ref().unwrap().encoding());
        assert!(tick_until(&mut app, Duration::from_secs(15), |a| {
            let c = a.cut.as_ref().unwrap(); !c.encoding() && !c.scanning()
        }));
        let cut = app.cut.as_ref().unwrap();
        assert!(cut.pending_gops().is_empty(), "{}", cut.status);
        assert_ne!(app.videos[0].info.path, clip);
        assert!(cut.keys.iter().any(|k| (*k-at).abs() < 1e-4));
        let working = app.videos[0].info.path.clone();
        let controls = app.controls();
        let all = controls.iter().find(|c| matches!(c.action, Action::Cut(CutAction::EncodeAll))).unwrap();
        assert_eq!(all.label, "Render");
        assert_eq!(all.hint, "⌘R");
        assert!(all.r.y < HEADER_H);
        // Empty batch is a no-op, not another generation of the source.
        app.key(Key::Render);
        assert!(!app.cut.as_ref().unwrap().encoding());
        drop(app); std::fs::remove_file(working).unwrap();
    }

    #[test]
    fn completed_encode_belongs_to_its_source_after_switching_modes() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 2);
        app.key(Key::Char('t'));
        app.set_cut_mode(false); app.select(1);
        let owned = app.cut_video;
        app.cut.as_mut().unwrap().reload = Some(clip.clone());
        app.videos[owned].linked_copy = true;
        app.videos[1].linked_copy = true;
        app.tick(0.01, (1280.0, 800.0), 2.0);
        assert!(!app.videos[owned].linked_copy);
        assert!(app.videos[1].linked_copy, "completion must not replace the newly focused source");
        app.set_cut_mode(true);
        assert_eq!(app.cut_video, 1, "even identical linked paths have independent edits");
    }

    /// ⌘-click adds a clip to the picks (and takes one out again) without
    /// moving the playhead; a drag from a vacant row sweeps a rubber band
    /// over every clip it touches; Backspace cuts the whole set as one undo
    /// step; a plain click on a vacant row lets go and seeks.
    #[test]
    fn cmd_click_and_rubber_band_pick_several_clips() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 1);
        assert!(tick_until(&mut app, Duration::from_secs(5), |a| a.started));
        app.key(Key::Char('t'));
        assert!(tick_until(&mut app, Duration::from_secs(10), |a| a.cut.as_ref().is_some_and(|c| !c.scanning() && c.pps > 0.0)));
        app.playing = false;
        for t in [1.0, 2.0, 3.0] { assert!(app.cut.as_mut().unwrap().split(t)); }
        assert_eq!(app.cut.as_ref().unwrap().segments().len(), 4);
        let lanes = app.cut_lanes();
        let v = lanes.video;
        let x_of = |app: &App, t: f64| { let c = app.cut.as_ref().unwrap(); v.x + ((t - c.t0) * c.pps) as f32 };

        app.mouse_down(x_of(&app, 0.5), v.y + v.h / 2.0);
        app.mouse_up();
        assert_eq!(app.cut_picks, vec![(0.0, 1.0, false)]);
        let held = app.t;
        app.mouse_down_with_mods(x_of(&app, 2.5), v.y + v.h / 2.0, false, true);
        app.mouse_up();
        assert_eq!(app.cut_picks, vec![(0.0, 1.0, false), (2.0, 3.0, false)], "⌘-click adds");
        assert_eq!(app.t, held, "⌘-click leaves the playhead");
        app.mouse_down_with_mods(x_of(&app, 2.5), v.y + v.h / 2.0, false, true);
        app.mouse_up();
        assert_eq!(app.cut_picks, vec![(0.0, 1.0, false)], "⌘-click again takes it out");

        // The band: from the caption row up across the picture row.
        let band_rows = app.cut_band_rows().unwrap();
        assert!(contains(band_rows, x_of(&app, 0.5), lanes.subs.y + 2.0));
        app.mouse_down(x_of(&app, 1.5), lanes.subs.y + 2.0);
        assert!(matches!(app.cut_drag, Some(CutDrag::Band { add: false, .. })));
        assert!(app.cut_picks.is_empty(), "a plain band starts from nothing");
        app.cursor_moved(x_of(&app, 3.5), v.y + v.h / 2.0);
        assert_eq!(app.cut_picks, vec![(1.0, 2.0, false), (2.0, 3.0, false), (3.0, 4.0, false)]);
        let mut items = Vec::new();
        app.build_cut_timeline(&mut items);
        assert!(items.iter().any(|i| matches!(i, Item::Rect(r) if r.border_w == 1.0 && r.border_color == ACCENT)), "the band is drawn");
        app.cursor_moved(x_of(&app, 2.5), v.y + v.h / 2.0);
        assert_eq!(app.cut_picks, vec![(1.0, 2.0, false), (2.0, 3.0, false)], "shrinking the band drops clips again");
        app.mouse_up();
        assert_eq!(app.cut_picks.len(), 2, "the picks outlive the band");
        assert_eq!(app.t, held, "a band does not move the playhead");

        // ⌘-band keeps what was picked.
        app.cut_picks = vec![(0.0, 1.0, false)];
        app.mouse_down_with_mods(x_of(&app, 2.5), lanes.subs.y + 2.0, false, true);
        app.cursor_moved(x_of(&app, 3.5), v.y + v.h / 2.0);
        app.mouse_up();
        assert_eq!(app.cut_picks, vec![(0.0, 1.0, false), (2.0, 3.0, false), (3.0, 4.0, false)]);

        // Backspace cuts them all; one undo brings them all back.
        app.key(Key::Backspace);
        assert_eq!(app.cut.as_ref().unwrap().cuts(), &[(0.0, 1.0), (2.0, 4.0)]);
        assert!(app.cut_picks.is_empty());
        app.key(Key::Undo);
        assert!(app.cut.as_ref().unwrap().cuts().is_empty(), "the set undoes as one step");

        // A click (no drag) on a vacant row lets go and takes the playhead.
        app.cut_picks = vec![(0.0, 1.0, false)];
        app.mouse_down(x_of(&app, 2.5), lanes.subs.y + 2.0);
        app.mouse_up();
        assert!(app.cut_picks.is_empty());
        assert!((app.t - 2.5).abs() < 0.1, "clicked at 2.5, playhead at {}", app.t);
        assert!(app.cut_drag.is_none());
    }

    #[test]
    fn pointer_seek_stays_exact_as_decoded_frames_arrive() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 1);
        assert!(tick_until(&mut app, Duration::from_secs(5), |a| a.started));
        app.key(Key::Char('t'));
        app.playing = false;
        let lane = app.cut_lanes().video;
        for fraction in [0.7137, 0.2237, 0.6137] {
            app.mouse_down(lane.x + lane.w * fraction, lane.y + lane.h / 2.0);
            let target = app.t;
            let start = Instant::now();
            while app.videos[0].pending && start.elapsed() < Duration::from_secs(5) {
                app.tick(0.01, (1280.0, 800.0), 2.0);
                assert_eq!(app.t, target, "decoder must not move a pointer-owned playhead");
                std::thread::sleep(Duration::from_millis(5));
            }
            assert!(!app.videos[0].pending, "seek completed");
            assert!((app.videos[0].shown_pts - target).abs() < 1.0 / 30.0, "the exact frame: {} vs {target}", app.videos[0].shown_pts);
            app.mouse_up();
            for _ in 0..5 { app.tick(0.01, (1280.0, 800.0), 2.0); assert_eq!(app.t, target); }
        }
    }

    #[test]
    fn holding_the_timeline_pauses_playback_until_release() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 1);
        assert!(tick_until(&mut app, Duration::from_secs(5), |a| a.started));
        app.key(Key::Char('t'));
        assert!(tick_until(&mut app, Duration::from_secs(10), |a| a.cut.as_ref().is_some_and(|c| !c.scanning())));
        app.playing = true;
        let l = app.cut_lanes();
        app.mouse_down(l.video.x + l.video.w * 0.5, l.ruler.y + 4.0);
        assert!(!app.playing, "the press holds the playhead");
        let held = app.t;
        app.cursor_moved(l.video.x + l.video.w * 0.25, l.ruler.y + 4.0);
        assert!(app.t < held && !app.playing, "the playhead follows the pointer");
        let there = app.t;
        for _ in 0..10 { app.tick(0.05, (1280.0, 800.0), 2.0); }
        assert!((app.t - there).abs() < 1e-6, "the clock stays put while the button is down");
        app.mouse_up();
        assert!(app.playing, "release lets playback continue");

        app.playing = false;
        app.mouse_down(l.video.x + 40.0, l.ruler.y + 4.0);
        app.mouse_up();
        assert!(!app.playing, "a paused clip stays paused");
    }

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
        app.scroll(0.0, 20.0);
        let zoomed = app.cut.as_ref().unwrap().pps;
        assert!(zoomed > before, "swiping down should zoom in: {before} -> {zoomed}");
        app.scroll(0.0, -20.0);
        assert!(app.cut.as_ref().unwrap().pps < zoomed, "swiping up should zoom back out");
        // Anywhere on the panel counts: the ruler, the gutter, the toolbar.
        let l = app.cut_lanes();
        for (x, y) in [(l.ruler.x + 40.0, l.ruler.y + 4.0), (l.video.x - 30.0, l.video.y + 10.0), (app.workspace().transport.x + 300.0, app.workspace().transport.y + 25.0)] {
            app.cursor_moved(x, y);
            let before = app.cut.as_ref().unwrap().pps;
            app.scroll(0.0, 20.0);
            assert!(app.cut.as_ref().unwrap().pps > before, "no zoom at ({x}, {y})");
        }
        // Pinch: spreading zooms in, closing zooms out.
        let before = app.cut.as_ref().unwrap().pps;
        let anchor_before = app.cut_time_at(app.cursor.0);
        app.pinch(0.2);
        assert!(app.cut.as_ref().unwrap().pps > before, "spreading should zoom in");
        assert!((app.cut_time_at(app.cursor.0) - anchor_before).abs() < 1e-6, "pinch preserves the time under the pointer");
        app.pinch(-0.2);
        assert!((app.cut.as_ref().unwrap().pps - before).abs() < 1e-6, "pinching inward should zoom back out");
    }

    /// The hover lift on a clip eases up over a few frames and back down after
    /// the pointer leaves; the loop stays hot only while it moves.
    #[test]
    fn cut_clip_hover_lift_tweens_in_and_out() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 1);
        assert!(tick_until(&mut app, Duration::from_secs(5), |a| a.started));
        app.key(Key::Char('t'));
        assert!(tick_until(&mut app, Duration::from_secs(10), |a| a.cut.as_ref().is_some_and(|c| !c.scanning())));
        app.key(Key::Space);
        let v = app.cut_lanes().video;
        app.cursor_moved(v.x + v.w * 0.5, v.y + v.h * 0.5);
        let (g, snd) = app.cut_clip_at(app.cursor.0, app.cursor.1).expect("a clip under the pointer");
        assert!(!snd);
        let d1 = app.tick(0.03, (1280.0, 800.0), 2.0);
        let l1 = app.cut_hot_level(g.start, false);
        assert!(l1 > 0.0 && l1 < 1.0, "rising: {l1}");
        assert!(d1.animating, "the loop stays hot while the lift rises");
        for _ in 0..10 { app.tick(0.03, (1280.0, 800.0), 2.0); }
        assert_eq!(app.cut_hot_level(g.start, false), 1.0);
        assert!(!app.cut_hot_animating(), "settled: no redraws needed");
        app.mouse_down(app.cursor.0, app.cursor.1);
        for _ in 0..10 { app.tick(0.03, (1280.0, 800.0), 2.0); }
        assert_eq!(app.cut_hot_level(g.start, false), 1.0, "pressing must not fade the hovered clip");
        app.mouse_up();
        app.tick(0.03, (1280.0, 800.0), 2.0);
        assert_eq!(app.cut_hot_level(g.start, false), 1.0);
        app.cursor_moved(v.x + v.w * 0.5, v.y - 200.0);
        app.tick(0.03, (1280.0, 800.0), 2.0);
        let l2 = app.cut_hot_level(g.start, false);
        assert!(l2 > 0.0 && l2 < 1.0, "falling: {l2}");
        for _ in 0..20 { app.tick(0.03, (1280.0, 800.0), 2.0); }
        assert_eq!(app.cut_hot_level(g.start, false), 0.0);
        assert!(app.cut_hot.is_empty(), "settled zeros are dropped");
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
        // The click on the lanes picked a clip: the first Esc lets go of it.
        app.key(Key::Escape);
        assert!(app.cut_mode && app.cut_picks.is_empty());
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

    #[test]
    fn backspace_removes_checked_sources_and_preserves_unselected_focus() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 4);
        app.playing = false;
        app.t = 0.5;
        app.active = 2;
        app.videos[0].selected = true;
        app.videos[3].selected = true;
        app.videos[2].linked_copy = true;
        app.key(Key::Backspace);
        assert_eq!(app.videos.len(), 2);
        assert_eq!(app.masks.len(), 2);
        assert_eq!(app.active, 1);
        assert!(app.videos[app.active].linked_copy);
        assert_eq!(app.t, 0.5);
        assert!(app.videos.iter().all(|v| v.pending));
        for v in &mut app.videos { v.selected = true; }
        app.key(Key::Backspace);
        assert!(!app.ready());
    }

    #[test]
    fn option_drag_inserts_at_drop_boundary_including_blank_space() {
        let Some(clip) = test_clip() else { return };
        for destination in [0, 1, 3] {
            let mut app = mk_app(&clip, 3);
            app.playing = false;
            app.tick(0.0, (1280.0, 1000.0), 2.0);
            let from = app.source_row(1);
            let x = from.x + from.w / 2.0;
            let y = if destination == 3 {
                app.source_row(2).y + SOURCE_H + 10.0
            } else { app.source_row(destination).y + 10.0 };
            assert!(contains(app.workspace().list, x, y));
            app.mouse_down_with_alt(x, from.y + 40.0, true);
            app.cursor_moved(x, y);
            app.mouse_released();
            assert_eq!(app.videos.len(), 4);
            assert_eq!(app.active, destination);
            assert!(app.videos[destination].linked_copy);
            assert_eq!(app.videos.iter().filter(|v| v.linked_copy).count(), 1);
        }
    }

    #[test]
    fn option_drag_duplicates_only_the_dragged_source_on_release() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 2);
        app.playing = false;
        app.tick(0.0, (1280.0, 800.0), 2.0);
        app.videos[1].selected = true;
        let row = app.source_row(0);
        let (x, y) = (row.x + row.w / 2.0, row.y + 40.0);
        app.mouse_down_with_alt(x, y, true);
        app.mouse_released();
        assert_eq!(app.videos.len(), 2, "Option-click is not a drag");
        let t = app.t;
        app.mouse_down_with_alt(x, y, true);
        app.cursor_moved(x + 10.0, y + 15.0);
        assert_eq!(app.videos.len(), 2, "no duplicate until release");
        app.mouse_released();
        assert_eq!(app.videos.len(), 3);
        assert_eq!(app.active, 0);
        assert!(app.videos[app.active].linked_copy && app.videos[app.active].selected);
        assert!(!app.videos[2].selected);
        assert_eq!(app.t, t);
        assert!(!app.playing);
        app.mouse_released();
        assert_eq!(app.videos.len(), 3, "one copy per gesture");
        app.mouse_down_with_alt(x, y, true);
        app.cursor_moved(x + 10.0, y + 15.0);
        app.cursor_left();
        app.mouse_released();
        assert_eq!(app.videos.len(), 3, "leaving cancels the gesture");
    }

    #[test]
    fn backspace_closes_focused_source_and_leaves_empty_window_open() {
        let Some(clip) = test_clip() else { return };
        let mut app = mk_app(&clip, 3);
        app.key(Key::Char('2'));
        app.key(Key::Backspace);
        assert_eq!(app.videos.len(), 2);
        assert_eq!(app.active, 1);
        assert!(app.take_cmds().iter().any(|c| matches!(c, Cmd::VideosChanged)));
        app.key(Key::Backspace);
        app.key(Key::Backspace);
        assert!(!app.ready());
        app.take_cmds();
        app.key(Key::Backspace);
        assert!(!app.take_cmds().iter().any(|c| matches!(c, Cmd::Quit)));
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
        let _ = app.tick(0.0, (1280.0, 800.0), 2.0);
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
            let _ = app.tick(0.010, (1280.0, 800.0), 2.0);
        }
        let advanced = app.t - t0;
        assert!(
            (advanced - 0.10 * 1.5625).abs() < 1e-6,
            "clock should run at 1.5625×: advanced {advanced:.5}"
        );
        app.key(Key::Char('m'));
        app.key(Key::Backspace);
        assert!((app.speed - 1.0).abs() < 1e-9);
        assert_eq!(app.videos.len(), 2, "mask mode retains speed reset");
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
/// A rubber band smaller than this on both axes is still a click.
const BAND_MIN: f32 = 4.0;
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
