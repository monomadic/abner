//! Minimal export workspace. Workers publish a bounded latest-frame mailbox; no GUI waits for rendering.
use crate::{
    app::{Key, hex_color, ui_label},
    player::Rgba,
    probe::VideoInfo,
    render::{Align, Item, RectItem, RectPx, Upload, VideoMode},
};
use abner_processor_api::{Cancellation, Registry};
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    time::Instant,
};

#[derive(Clone)]
pub(crate) struct Rendered {
    pub pixels: Rgba,
    pub dims: [u32; 2],
    pub source_pts: f64,
    pub output_time: f64,
    pub count: u64,
    pub frame_duration: f64,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Action {
    Preset(bool),
    Lossless,
    Container(bool),
    Export,
    Destination,
    Cancel,
    Compare,
    Follow,
    Seek(f64),
    Reveal,
}
struct Job {
    latest: Arc<Mutex<Option<Rendered>>>,
    done: mpsc::Receiver<Result<PathBuf, String>>,
    cancel: Cancellation,
    active: Arc<Mutex<Vec<Cancellation>>>,
    source: PathBuf,
    copy_cancel: Arc<std::sync::atomic::AtomicBool>,
}
pub(crate) struct Page {
    pub(crate) inputs:crate::spatial::JobInputs,
    /// Chapters in SOURCE time for the next job: the cut's list when the
    /// timeline owns this clip, `None` to carry the source file's own.
    pub(crate) chapters: Option<Vec<crate::cut::Chapter>>,
    pub open: bool,
    pub crop: Option<(u32, u32, u32, u32)>,
    pub prores: bool,
    pub lossless: bool,
    pub copy_unavailable: Option<&'static str>,
    pub mkv: bool,
    pub following: bool,
    pub original: bool,
    job: Option<Job>,
    frames: VecDeque<Rendered>,
    selected: usize,
    held: Option<Rendered>,
    started: Instant,
    elapsed: f64,
    pub status: String,
    destination: Option<PathBuf>,
    path: Option<PathBuf>,
    pub duration: f64,
    count: u64,
    rendered_time: f64,
    source: Option<PathBuf>,
    pub seek: Option<f64>,
    dirty: bool,
}
impl Default for Page {
    fn default() -> Self {
        Self {
            inputs:Default::default(),
            chapters: None,
            open: false,
            crop: None,
            prores: false,
            lossless: false,
            copy_unavailable: None,
            mkv: false,
            following: true,
            original: false,
            job: None,
            frames: VecDeque::new(),
            selected: 0,
            held: None,
            started: Instant::now(),
            elapsed: 0.,
            status: "Choose a preset and export the enabled pipeline.".into(),
            destination: None,
            path: None,
            duration: 0.,
            count: 0,
            rendered_time: 0.,
            source: None,
            seek: None,
            dirty: false,
        }
    }
}
impl Drop for Page {
    fn drop(&mut self) {
        self.cancel();
    }
}
#[derive(Clone, Copy)]
pub(crate) struct Layout {
    pub panel: RectPx,
    pub sidebar: RectPx,
    pub video: RectPx,
    pub context: RectPx,
    pub transport: RectPx,
    pub progress: RectPx,
    pub footer: RectPx,
    pub seek: RectPx,
}
impl Layout {
    pub fn new(vp: (f32, f32)) -> Self {
        let side = if vp.0 < 1000. { 220. } else { 332. };
        let panel = RectPx {
            x: 16.,
            y: 60.,
            w: (vp.0 - side - 48.).max(280.),
            h: (vp.1 - 114.).max(280.),
        };
        let sidebar = RectPx {
            x: panel.x + panel.w + 16.,
            y: 60.,
            w: side,
            h: panel.h,
        };
        let pad = if vp.0 < 1000. { 12. } else { 24. };
        let context = RectPx {
            x: panel.x + pad,
            y: panel.y + 16.,
            w: panel.w - 2. * pad,
            h: 36.,
        };
        let video = RectPx {
            x: context.x,
            y: context.y + 48.,
            w: context.w,
            h: (panel.h - 220.).max(80.),
        };
        let transport = RectPx {
            x: video.x,
            y: video.y + video.h + 12.,
            w: video.w,
            h: 64.,
        };
        let seek = RectPx {
            x: transport.x + transport.w * 0.32,
            y: transport.y + 42.,
            w: transport.w * 0.46,
            h: 6.,
        };
        let progress = RectPx {
            x: video.x,
            y: transport.y + 100.,
            w: video.w,
            h: 7.,
        };
        Self {
            panel,
            sidebar,
            video,
            context,
            transport,
            progress,
            seek,
            footer: RectPx {
                x: 0.,
                y: vp.1 - 38.,
                w: vp.0,
                h: 38.,
            },
        }
    }
}
fn hit(r: RectPx, x: f32, y: f32) -> bool {
    x >= r.x && y >= r.y && x < r.x + r.w && y < r.y + r.h
}
impl Page {
    fn settings_geometry(l: Layout) -> (f32, f32, f32, f32) {
        let step = ((l.sidebar.h - 84.) / 12.).clamp(22., 44.);
        (l.sidebar.x + 20., l.sidebar.w - 40., l.sidebar.y + 65., step)
    }
    fn destination_name(&self) -> String {
        let ext = if self.mkv { "mkv" } else { "mp4" };
        let mut path = self.destination.clone().unwrap_or_else(|| {
            let stem = self.source.as_ref().and_then(|p| p.file_stem()).unwrap_or_default().to_string_lossy();
            PathBuf::from(format!("{stem}.export.{ext}"))
        });
        path.set_extension(ext);
        path.file_name().unwrap_or_default().to_string_lossy().into_owned()
    }
    pub fn set_copy_unavailable(&mut self, reason: Option<&'static str>) {
        self.copy_unavailable = reason;
        if reason.is_some() { self.lossless = false; }
    }
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }
    pub fn cancel(&mut self) {
        if let Some(j) = &self.job {
            j.cancel.cancel();
            j.copy_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            for c in j.active.lock().unwrap().iter() {
                c.cancel();
            }
            self.status = "Cancelling export…".into();
        }
    }
    pub fn reset_source(&mut self, info: &VideoInfo) {
        if self.source.as_ref() != Some(&info.path) {
            self.cancel();
            self.frames.clear();
            self.held = None;
            self.crop = None;
            self.selected = 0;
            self.path = None;
            self.count = 0;
            self.rendered_time = 0.;
            self.destination = None;
            self.source = Some(info.path.clone());
            self.duration = info.duration;
            self.dirty = true;
        }
    }
    pub fn start(
        &mut self,
        info: VideoInfo,
        keeps: Vec<(f64, f64)>,
        registry: Arc<Mutex<Registry>>,
        chain: Vec<(String, serde_json::Value)>,
    ) {
        if self.busy() {
            return;
        }
        if self.lossless && (self.copy_unavailable.is_some() || !chain.is_empty()
            || self.crop.is_some() || !self.inputs.crops.is_empty()) {
            self.status = "Lossless copy is unavailable: these edits require rendering.".into();
            return;
        }
        // ProRes in MP4 is not interoperable. MKV is the supported ProRes container here.
        if !self.lossless && self.prores && !self.mkv {
            self.status = "ProRes requires MKV. Select MKV to export.".into();
            return;
        }
        let ext = if self.mkv { "mkv" } else { "mp4" };
        let Some(mut dest) = self.destination.clone().or_else(||
            crate::open::export_destination(&self.destination_name())) else { return; };
        dest.set_extension(ext);
        self.destination = Some(dest.clone());
        if dest.exists() {
            self.status = "Output already exists. Choose a new filename.".into();
            return;
        }
        self.frames.clear();
        self.held = None;
        self.selected = 0;
        self.count = 0;
        self.rendered_time = 0.;
        self.path = None;
        self.following = true;
        self.original = false;
        self.duration = keeps.iter().map(|(a, b)| b - a).sum();
        self.started = Instant::now();
        self.elapsed = 0.;
        self.status = if self.lossless { "Copying compressed media…" } else { "Preparing pipeline…" }.into();
        let cancel = Cancellation::default();
        let latest = Arc::new(Mutex::new(None));
        let (tx, done) = mpsc::sync_channel(1);
        let shared = latest.clone();
        let stop = cancel.clone();
        let inputs = self.inputs.clone();
        let chapters = self.chapters.clone();
        let prores = self.prores;
        let lossless = self.lossless;
        let crop = self.crop;
        let active = Arc::new(Mutex::new(Vec::new()));
        let worker_active = active.clone();
        let source = info.path.clone();
        let copy_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_copy_cancel = copy_cancel.clone();
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let chapters = match chapters {
                    Some(list) => list,
                    None => crate::media_metadata::timeline(&info.path)?
                        .map(|t| t.chapters.into_iter()
                            .map(|(start, title)| crate::cut::Chapter { start, title }).collect())
                        .unwrap_or_default(),
                };
                let chapters = crate::cut::output_chapters(&chapters, &keeps);
                if lossless {
                    stop.check().map_err(|e| e.to_string())?;
                    crate::media_io::export_with_cancel(&info.path, &dest, &keeps, true, &chapters, None, &worker_copy_cancel)
                        .map_err(|e| e.to_string())?;
                    return Ok(dest);
                }
                let registry = registry
                    .lock()
                    .map_err(|_| "Processor registry unavailable".to_string())?;
                crate::media_io::pipeline_export_with_inputs(
                    &inputs,
                    &info,
                    &dest,
                    &keeps,
                    &chapters,
                    &registry,
                    &chain,
                    prores,
                    crop,
                    &stop,
                    &worker_active,
                    |frame| {
                        stop.check()?;
                        *shared.lock().unwrap() = Some(frame);
                        Ok(())
                    },
                )
                .map_err(|e| e.to_string())?;
                Ok(dest)
            }))
            .unwrap_or_else(|_| Err("Export worker panicked".into()));
            let _ = tx.send(result);
        });
        self.job = Some(Job {
            latest,
            done,
            cancel,
            active,
            source,
            copy_cancel,
        });
    }
    pub fn poll(&mut self, uploads: &mut Vec<Upload>, slot: usize) {
        if let Some(job) = &self.job {
            if let Some(frame) = job
                .latest
                .lock()
                .unwrap()
                .take()
                .filter(|_| self.source.as_ref() == Some(&job.source))
            {
                self.count = frame.count;
                self.rendered_time = frame.output_time;
                self.frames.push_back(frame);
                let mut bytes: usize = self.frames.iter().map(|f| f.pixels.len()).sum();
                while bytes > 128 * 1024 * 1024 && self.frames.len() > 1 {
                    bytes -= self.frames.pop_front().unwrap().pixels.len();
                    self.selected = self.selected.saturating_sub(1);
                }
                if self.following {
                    self.selected = self.frames.len() - 1;
                    self.held = Some(self.frames[self.selected].clone());
                    self.seek = Some(self.frames[self.selected].source_pts);
                    self.dirty = true;
                }
                self.status = if job.cancel.check().is_err() {
                    "Cancelling export…"
                } else if self.rendered_time >= self.duration - 0.001 {
                    "Finalizing audio and container…"
                } else {
                    "Rendering and encoding…"
                }
                .into();
            }
        }
        let result = self.job.as_ref().and_then(|j| match j.done.try_recv() {
            Ok(r) => Some(r),
            Err(mpsc::TryRecvError::Disconnected) => Some(Err("Export worker stopped".into())),
            Err(mpsc::TryRecvError::Empty) => None,
        });
        if let Some(result) = result {
            self.elapsed = self.started.elapsed().as_secs_f64();
            let same_source = self
                .job
                .as_ref()
                .is_some_and(|j| self.source.as_ref() == Some(&j.source));
            self.job = None;
            if !same_source {
                self.status = "Source changed; export cancelled.".into();
                return;
            }
            match result {
                Ok(path) => {
                    self.status = format!("Exported {}", path.display());
                    self.path = Some(path);
                }
                Err(e) => self.status = format!("Export stopped: {e}"),
            }
        }
        if self.dirty && self.open {
            if let Some(f) = self.current() {
                uploads.push(Upload {
                    idx: slot,
                    w: f.dims[0],
                    h: f.dims[1],
                    buf: f.pixels.clone(),
                });
            }
            self.dirty = false;
        }
    }
    fn current(&self) -> Option<&Rendered> {
        if self.following {
            self.frames.get(self.selected)
        } else {
            self.held
                .as_ref()
                .or_else(|| self.frames.get(self.selected))
        }
    }
    pub fn enter(&mut self) {
        self.open = true;
        self.dirty = true;
    }
    pub fn key(&mut self, k: Key) -> bool {
        match k {
            Key::Escape if self.busy() => {
                self.cancel();
                true
            }
            Key::Tab => {
                self.original = !self.original;
                true
            }
            Key::Left | Key::Right => {
                self.following = false;
                self.selected = if k == Key::Left {
                    self.selected.saturating_sub(1)
                } else {
                    (self.selected + 1).min(self.frames.len().saturating_sub(1))
                };
                if let Some(f) = self.frames.get(self.selected) {
                    self.held = Some(f.clone());
                    self.seek = Some(f.source_pts);
                    self.dirty = true;
                }
                true
            }
            _ => false,
        }
    }
    pub fn actions(&self, vp: (f32, f32)) -> Vec<(RectPx, &'static str, bool, Action)> {
        let l = Layout::new(vp);
        let (x, w, y, step) = Self::settings_geometry(l);
        let third = (w - 12.) / 3.;
        let half = (w - 6.) / 2.;
        let r = |x, y, w| RectPx { x, y, w, h: step * 0.78 };
        vec![
            (r(x, y + step, third), "Copy", self.lossless, Action::Lossless),
            (r(x + third + 6., y + step, third), "HEVC", !self.prores && !self.lossless, Action::Preset(false)),
            (r(x + 2. * (third + 6.), y + step, third), "ProRes", self.prores && !self.lossless, Action::Preset(true)),
            (r(x, y + 3. * step, half), "MKV", self.mkv, Action::Container(true)),
            (r(x + half + 6., y + 3. * step, half), "MP4", !self.mkv, Action::Container(false)),
            (r(x, y + 5.6 * step, w), "", false, Action::Destination),
            (r(x, y + 7.1 * step, w), "", false, Action::Destination),
            (r(x, y + 11. * step, w), "Export", false, Action::Export),
            (
                r(
                    l.context.x + l.context.w * 0.28,
                    l.context.y,
                    l.context.w * 0.23,
                ),
                "TAB · Original / Output",
                false,
                Action::Compare,
            ),
            (
                r(
                    l.context.x + l.context.w * 0.53,
                    l.context.y,
                    l.context.w * 0.21,
                ),
                if self.following {
                    "Following render"
                } else {
                    "Follow render"
                },
                self.following,
                Action::Follow,
            ),
            (
                r(16., l.footer.y + 5., 120.),
                "Export",
                false,
                Action::Export,
            ),
            (
                r(148., l.footer.y + 5., 100.),
                "Cancel",
                false,
                Action::Cancel,
            ),
            (
                r(260., l.footer.y + 5., 150.),
                "Reveal output",
                false,
                Action::Reveal,
            ),
        ]
    }
    pub fn click(&self, vp: (f32, f32), x: f32, y: f32) -> Option<Action> {
        let l = Layout::new(vp);
        if hit(
            RectPx {
                y: l.seek.y - 12.,
                h: 30.,
                ..l.seek
            },
            x,
            y,
        ) {
            return Some(Action::Seek(
                ((x - l.seek.x) / l.seek.w).clamp(0., 1.) as f64
            ));
        }
        self.actions(vp)
            .into_iter()
            .find(|(r, ..)| hit(*r, x, y))
            .map(|(.., a)| a)
    }
    pub fn apply(&mut self, a: Action) -> Option<PathBuf> {
        match a {
            Action::Preset(p) if !self.busy() => {
                self.lossless = false;
                self.prores = p;
                if p {
                    self.mkv = true;
                }
            }
            Action::Lossless if !self.busy() && self.copy_unavailable.is_none() => {
                self.lossless = true;
                self.prores = false;
            }
            Action::Container(m) if !self.busy() => {
                if !m && self.prores && !self.lossless {
                    self.status = "ProRes requires MKV.".into();
                } else {
                    self.mkv = m;
                }
            }
            Action::Destination if !self.busy() => {
                if let Some(mut path) = crate::open::export_destination(&self.destination_name()) {
                    path.set_extension(if self.mkv { "mkv" } else { "mp4" });
                    self.destination = Some(path);
                }
            }
            Action::Cancel => self.cancel(),
            Action::Compare => self.original = !self.original,
            Action::Follow => {
                self.following = true;
                self.held = None;
                self.selected = self.frames.len().saturating_sub(1);
                if let Some(f) = self.frames.back() {
                    self.seek = Some(f.source_pts);
                    self.dirty = true;
                }
            }
            Action::Seek(f) => {
                self.following = false;
                if !self.frames.is_empty() {
                    self.selected = (f * (self.frames.len() - 1) as f64).round() as usize;
                    self.seek = Some(self.frames[self.selected].source_pts);
                    self.dirty = true;
                }
            }
            Action::Reveal => return self.path.clone(),
            _ => {}
        }
        None
    }
    pub fn draw(
        &self,
        items: &mut Vec<Item>,
        vp: (f32, f32),
        source: usize,
        slot: usize,
        info: &VideoInfo,
        cursor: (f32, f32),
        shown_pts: f64,
        show_ui: bool,
    ) {
        if !show_ui {
            let f = self.current();
            let dims = f
                .filter(|_| !self.original)
                .map_or((info.width, info.height), |f| (f.dims[0], f.dims[1]));
            let idx = if f.is_some() && !self.original {
                slot
            } else {
                source
            };
            let r = RectPx {
                x: 0.,
                y: 0.,
                w: vp.0,
                h: vp.1,
            };
            items.push(Item::Rect(RectItem::new(r, hex_color(0x0c0d0d))));
            if !self.original
                || f.is_none_or(|f| (f.source_pts - shown_pts).abs() <= 0.51 / info.fps.max(1.))
            {
                items.push(Item::Video {
                    a: idx,
                    b: idx,
                    r: crate::app::App::fit_rect(dims, r),
                    uv: [0., 0., 1., 1.],
                    mode: VideoMode::Tex,
                    p0: 0.,
                    p1: 0.,
                });
            }
            return;
        }
        let l = Layout::new(vp);
        let ink = hex_color(0xededec);
        let dim = hex_color(0xaaaaa6);
        let blue = hex_color(0x008cff);
        items.push(Item::Rect(RectItem::new(
            RectPx {
                x: 0.,
                y: 44.,
                w: vp.0,
                h: vp.1 - 44.,
            },
            hex_color(0x111212),
        )));
        for r in [l.panel, l.sidebar] {
            items.push(Item::Rect(RectItem {
                radius: 6.,
                border_w: 1.,
                border_color: hex_color(0x252626),
                ..RectItem::new(r, hex_color(0x0c0d0d))
            }));
        }
        let label = |items: &mut Vec<Item>, r: RectPx, text: &str, color| {
            ui_label(items, r.x, r.y, 12., color, text, Align::Left, r.w)
        };
        label(
            items,
            RectPx {
                w: l.context.w * 0.26,
                y: l.context.y + 18.,
                ..l.context
            },
            if self.original {
                "ORIGINAL / MATCHED FRAME"
            } else if self.following {
                "OUTPUT / LATEST FRAME"
            } else {
                "OUTPUT / MANUAL REVIEW"
            },
            ink,
        );
        let current = self.current();
        let fps = current.map_or(info.fps, |f| 1. / f.frame_duration);
        if l.context.w > 650. {
            ui_label(
                items,
                l.context.x + l.context.w * 0.77,
                l.context.y + 18.,
                10.,
                dim,
                format!("{} × {} / {:.0} fps", info.width, info.height, fps),
                Align::Left,
                l.context.w * 0.23,
            );
        }
        let dims = current
            .filter(|_| !self.original)
            .map_or((info.width, info.height), |f| (f.dims[0], f.dims[1]));
        let idx = if current.is_some() && !self.original {
            slot
        } else {
            source
        };
        let picture = crate::app::App::fit_rect(dims, l.video);
        items.push(Item::Clip(Some(l.video)));
        if self.original
            && current.is_some_and(|f| (f.source_pts - shown_pts).abs() > 0.51 / info.fps.max(1.))
        {
            items.push(Item::Rect(RectItem::new(l.video, hex_color(0x0c0d0d))));
            ui_label(
                items,
                l.video.x + l.video.w / 2.,
                l.video.y + l.video.h / 2.,
                12.,
                dim,
                "Seeking matching original…",
                Align::Center,
                l.video.w,
            );
        } else {
            items.push(Item::Video {
                a: idx,
                b: idx,
                r: picture,
                uv: [0., 0., 1., 1.],
                mode: VideoMode::Tex,
                p0: 0.,
                p1: 0.,
            });
        }
        items.push(Item::Clip(None));
        let lcd = RectPx {
            w: l.transport.w * 0.28,
            ..l.transport
        };
        items.push(Item::Rect(RectItem {
            radius: 5.,
            ..RectItem::new(lcd, hex_color(0x304b39))
        }));
        ui_label(
            items,
            lcd.x + 12.,
            lcd.y + 10.,
            10.,
            hex_color(0xa5c877),
            format!("OUTPUT TIME / {:.0} FPS", fps),
            Align::Left,
            lcd.w - 24.,
        );
        let time = current.map_or(0., |f| (f.output_time - f.frame_duration).max(0.));
        let whole = time.floor() as u64;
        let frame = ((time - time.floor()) * fps).round() as u64;
        items.push(Item::Text(crate::render::TextItem {
            valign: crate::render::VAlign::Middle,
            max_width: Some(lcd.w - 24.),
            ..crate::render::TextItem::new(
                lcd.x + 12.,
                lcd.y + 38.,
                20.,
                hex_color(0xa5c877),
                format!(
                    "{:02}:{:02}:{:02}:{:02}",
                    whole / 3600,
                    whole / 60 % 60,
                    whole % 60,
                    frame
                ),
            )
        }));
        ui_label(
            items,
            l.seek.x,
            l.transport.y + 12.,
            10.,
            dim,
            format!(
                "Recent rendered frames · {:.2}s / {:.2}s",
                self.rendered_time, self.duration
            ),
            Align::Left,
            l.seek.w,
        );
        items.push(Item::Rect(RectItem::new(l.seek, hex_color(0x292b2b))));
        let fraction = if self.frames.len() > 1 {
            self.selected as f32 / (self.frames.len() - 1) as f32
        } else {
            0.
        };
        if fraction > 0. {
            items.push(Item::Rect(RectItem::new(
                RectPx {
                    w: l.seek.w * fraction,
                    ..l.seek
                },
                blue,
            )));
        }
        let elapsed = if self.busy() {
            self.started.elapsed().as_secs_f64()
        } else {
            self.elapsed
        };
        let fps = self.count as f64 / elapsed.max(0.001);
        let done = (self.rendered_time / self.duration.max(0.001)).clamp(0., 1.);
        let eta = if done > 0. && self.busy() {
            format!("~{:.0}s remaining", elapsed * (1. - done) / done)
        } else {
            String::new()
        };
        ui_label(
            items,
            l.progress.x,
            l.progress.y - 22.,
            11.,
            ink,
            format!(
                "RENDER + ENCODE    {:.1}% · {} frames   {:.1} fps   {:.0}s elapsed   {}",
                done * 100.,
                self.count,
                fps,
                elapsed,
                eta
            ),
            Align::Left,
            l.progress.w,
        );
        items.push(Item::Rect(RectItem {
            radius: 3.,
            ..RectItem::new(l.progress, hex_color(0x292b2b))
        }));
        if done > 0. {
            items.push(Item::Rect(RectItem {
                radius: 3.,
                ..RectItem::new(
                    RectPx {
                        w: l.progress.w * done as f32,
                        ..l.progress
                    },
                    blue,
                )
            }));
        }
        ui_label(
            items,
            l.progress.x,
            l.progress.y + 22.,
            10.,
            dim,
            if self.actions(vp).iter().any(|(r, _, _, a)| *a == Action::Lossless && hit(*r, cursor.0, cursor.1)) {
                self.copy_unavailable.unwrap_or(&self.status)
            } else { &self.status },
            Align::Left,
            l.progress.w,
        );
        let (x, w, y, step) = Self::settings_geometry(l);
        let text = |items: &mut Vec<Item>, yy, size, color, value: &str| {
            ui_label(items, x, yy, size, color, value, Align::Left, w);
        };
        text(items, l.sidebar.y + 25., 18., ink, "Export Settings");
        for (offset, title) in [(0., "Codec"), (2., "Container"), (4.4, "Destination")] {
            text(items, y + offset * step, 15., ink, title);
        }
        for offset in [-0.55, 1.8, 3.95, 8.3] {
            items.push(Item::Rect(RectItem::new(RectPx { x, y: y + offset * step, w, h: 1. }, hex_color(0x292b29))));
        }
        text(items, y + 5.25 * step, 12., dim, "Filename");
        text(items, y + 6.75 * step, 12., dim, "Directory · click to choose");
        let dims = self.current().map(|f| (f.dims[0], f.dims[1]))
            .or_else(|| self.crop.map(|(_, _, w, h)| (w, h)))
            .unwrap_or((info.width, info.height));
        let codec = if self.lossless { info.codec.as_str() } else if self.prores { "ProRes" } else { "HEVC" };
        for (offset, key, value) in [
            (8.85, "Video", format!("{codec} · {} × {}", dims.0, dims.1)),
            (9.6, "Frame rate", format!("{fps:.2} fps")),
            (10.35, "Duration", format!("{:02}:{:02}", self.duration as u64 / 60, self.duration as u64 % 60)),
        ] {
            ui_label(items, x, y + offset * step, 12., dim, key, Align::Left, w * 0.35);
            ui_label(items, x + w, y + offset * step, 12., ink, value, Align::Right, w * 0.65);
        }
        items.push(Item::Rect(RectItem::new(l.footer, hex_color(0x0b0c0c))));
        for (r, t, selected, a) in self.actions(vp) {
            let hovered = hit(r, cursor.0, cursor.1);
            let disabled = match a {
                Action::Export => self.busy(),
                Action::Cancel => !self.busy(),
                Action::Reveal => self.path.is_none(),
                Action::Lossless => self.busy() || self.copy_unavailable.is_some(),
                Action::Container(false) => self.busy() || (self.prores && !self.lossless),
                Action::Preset(_) | Action::Container(true) | Action::Destination => self.busy(),
                _ => false,
            };
            if r.y >= l.footer.y {
                let r = RectPx { h: 28., ..r };
                items.push(Item::Rect(RectItem {
                    radius: 7.,
                    border_w: 1.,
                    border_color: hex_color(if disabled {
                        0x252525
                    } else if hovered {
                        0x666666
                    } else {
                        0x3b3d3b
                    }),
                    ..RectItem::new(r, hex_color(0x0b0c0c))
                }));
                ui_label(
                    items,
                    r.x + 10.,
                    r.y + r.h / 2.,
                    12.,
                    if disabled { hex_color(0x686868) } else { ink },
                    t,
                    Align::Left,
                    r.w - 20.,
                );
            } else {
                items.push(Item::Rect(RectItem {
                    radius: 7.,
                    border_w: 1.,
                    border_color: hex_color(if selected { 0x008cff } else { 0x3b3d3b }),
                    ..RectItem::new(
                        r,
                        hex_color(if disabled {
                            0x151715
                        } else if selected || a == Action::Export {
                            0x008cff
                        } else {
                            0x222422
                        }),
                    )
                }));
                ui_label(
                    items,
                    r.x + 10.,
                    r.y + r.h / 2.,
                    12.,
                    if disabled { dim } else { ink },
                    if a == Action::Destination {
                        if r.y < y + 6. * step { self.destination_name() }
                        else { self.destination.as_ref().and_then(|p| p.parent()).or_else(|| info.path.parent()).map(|p| p.display().to_string()).unwrap_or_default() }
                    } else { t.to_string() },
                    Align::Left,
                    r.w - 20.,
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use abner_processor_api::{Descriptor, Factory, Frame, Progress, Session};
    struct Red;
    struct RedSession {
        next_time: Option<(i64, [u32; 2])>,
        double: bool,
        dims: [usize; 2],
        cancel: Cancellation,
    }
    impl Factory for Red {
        fn descriptor(&self) -> Descriptor {
            Descriptor {
                id: "test.red".into(),
                name: "Red".into(),
                settings_schema: serde_json::json!({}),
                presets: vec![],
            }
        }
        fn create(
            &self,
            w: usize,
            h: usize,
            settings: &serde_json::Value,
        ) -> Result<Box<dyn Session>, String> {
            Ok(Box::new(RedSession {
                next_time: None,
                double: settings["double"].as_bool().unwrap_or(false),
                dims: [w, h],
                cancel: Cancellation::default(),
            }))
        }
    }
    struct Unsupported(bool);
    impl Factory for Unsupported {
        fn descriptor(&self) -> Descriptor {
            let mut d = Red.descriptor();
            d.id = "test.unsupported".into();
            d
        }
        fn capabilities(&self) -> abner_processor_api::Capabilities {
            use abner_processor_api::{Capabilities, InputRequirement, Role};
            if self.0 { Capabilities { role: Role::Generator, input: InputRequirement::IndependentFrame } }
            else { Capabilities { role: Role::Processor, input: InputRequirement::WholeClip } }
        }
        fn create(&self, _: usize, _: usize, _: &serde_json::Value) -> Result<Box<dyn Session>, String> {
            panic!("unsupported export capability must be rejected before creating a session")
        }
    }
    impl Session for RedSession {
        fn process(&mut self, mut f: Frame) -> Result<Vec<Frame>, String> {
            if self
                .next_time
                .is_some_and(|next| next != (f.pts, f.time_base))
            {
                return Err("non-contiguous timing; create a new session at cuts or seeks".into());
            }
            self.next_time = Some((f.pts + f.duration, f.time_base));
            for p in f.pixels.chunks_exact_mut(3) {
                p.copy_from_slice(&[65535, 0, 0]);
            }
            if self.double {
                let half = f.duration / 2;
                let second = Frame {
                    width: f.width,
                    height: f.height,
                    pixels: f.pixels.clone(),
                    pts: f.pts + half,
                    duration: f.duration - half,
                    time_base: f.time_base,
                };
                f.duration = half;
                Ok(vec![f, second])
            } else {
                Ok(vec![f])
            }
        }
        fn finish(&mut self) -> Result<Vec<Frame>, String> {
            Ok(vec![])
        }
        fn output_dimensions(&self) -> [usize; 2] {
            self.dims
        }
        fn cancellation(&self) -> Cancellation {
            self.cancel.clone()
        }
        fn progress(&self) -> Progress {
            Progress {
                phase: "test".into(),
                completed: 0,
                total: None,
            }
        }
    }
    #[test]
    fn lossless_selection_tracks_edit_eligibility() {
        let mut page = Page::default();
        page.apply(Action::Lossless);
        assert!(page.lossless);
        page.set_copy_unavailable(Some("Crop edits require rendering"));
        assert!(!page.lossless);
        page.apply(Action::Lossless);
        assert!(!page.lossless);
        page.set_copy_unavailable(None);
        page.apply(Action::Lossless);
        assert!(page.lossless);
        page.apply(Action::Preset(true));
        assert!(!page.lossless);
        assert!(page.prores);
        page.apply(Action::Lossless);
        page.apply(Action::Container(false));
        assert!(!page.mkv, "the render preset must not restrict stream copy");
    }

    #[test]
    fn manual_review_stays_put_while_latest_progress_advances() {
        let mut p = Page::default();
        p.open = true;
        p.source = Some(PathBuf::from("source"));
        let frame = |n| Rendered {
            pixels: Arc::new(vec![0; 16]),
            dims: [2, 2],
            source_pts: n as f64,
            output_time: n as f64,
            count: n,
            frame_duration: 1.,
        };
        let latest = Arc::new(Mutex::new(Some(frame(1))));
        let (_tx, done) = mpsc::channel();
        p.job = Some(Job {
            latest: latest.clone(),
            done,
            cancel: Cancellation::default(),
            active: Arc::default(),
            source: PathBuf::from("source"),
            copy_cancel: Arc::default(),
        });
        let mut uploads = vec![];
        p.poll(&mut uploads, 3);
        assert_eq!(p.seek.take(), Some(1.));
        p.apply(Action::Seek(0.));
        *latest.lock().unwrap() = Some(frame(2));
        p.poll(&mut uploads, 3);
        assert_eq!(p.frames[p.selected].source_pts, 1.);
        assert_eq!(p.count, 2);
        assert!(!p.following);
        p.apply(Action::Follow);
        assert_eq!(p.seek, Some(2.));
        assert!(p.following);
        p.key(Key::Tab);
        assert!(p.original);
    }
    #[test]
    fn minimal_controls_fit_and_footer_only_has_export_actions() {
        let p = Page::default();
        for vp in [(720., 480.), (1280., 800.), (1440., 900.)] {
            let l = Layout::new(vp);
            assert!(l.sidebar.x + l.sidebar.w <= vp.0);
            assert!(l.progress.y + 7. < l.footer.y);
            for (r, _, _, a) in p.actions(vp) {
                assert!(r.x >= 0. && r.x + r.w <= vp.0 + 0.1);
                if r.y >= l.footer.y {
                    assert!(matches!(
                        a,
                        Action::Export | Action::Cancel | Action::Reveal
                    ));
                }
            }
        }
    }
    #[test]
    fn native_pipeline_writes_selected_codecs_containers_audio_and_processed_pixels() {
        let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .is_test(true)
            .try_init();
        let dir = std::env::temp_dir().join(format!("abner-pipeline-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("input.mp4");
        let srt = dir.join("input.srt");
        std::fs::write(&srt, "1\n00:00:00,100 --> 00:00:00,500\nRendered cue\n").unwrap();
        let status = std::process::Command::new("ffmpeg")
            .args([
                "-y",
                "-v",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=black:size=320x180:rate=24:duration=0.5",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=440:duration=0.5",
                "-i",
            ])
            .arg(&srt)
            .args([
                "-map",
                "0",
                "-map",
                "1",
                "-map",
                "2",
                "-c:s",
                "mov_text",
                "-metadata",
                "title=Pipeline title",
                "-c:v",
                "libx264",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-shortest",
            ])
            .arg(&input)
            .status()
            .expect("ffmpeg test fixture");
        assert!(status.success());
        let info = crate::probe::probe(&input).unwrap();
        let mut registry = Registry::default();
        registry.register(Box::new(Red)).unwrap();
        for (prores, ext, codec) in [
            (false, "mp4", "hevc"),
            (false, "mkv", "hevc"),
            (true, "mkv", "prores"),
        ] {
            let dest = dir.join(format!("{}.{}", codec, ext));
            let _ = std::fs::remove_file(&dest);
            let mut previews = 0;
            crate::media_io::pipeline_export_with_inputs(
                &Default::default(),
                &info,
                &dest,
                &[(0., 0.5)],
                &[(0., 0.25, "One".into()), (0.25, 0.5, "Two".into())],
                &registry,
                &[("test.red".into(), serde_json::json!({}))],
                prores,
                None,
                &Cancellation::default(),
                &Mutex::default(),
                |f| {
                    previews += 1;
                    assert_eq!(&f.pixels[..4], &[255, 0, 0, 255]);
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(previews, 12);
            // Rendered exports carry chapters like the copy path does.
            let chapters = crate::media_metadata::timeline(&dest).unwrap().unwrap().chapters;
            assert_eq!(chapters, vec![(0., "One".into()), (0.25, "Two".into())], "{ext}");
            // ...and the source's subtitles and title.
            let cues = crate::media_io::cues(&dest).unwrap();
            assert_eq!(cues.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(), ["Rendered cue"], "{ext}");
            let title = if ext == "mkv" { fastmkv::read(&dest).unwrap().title } else {
                let src = crate::fc::reader(&dest).unwrap();
                filmcraft_isobmff::open(&src).unwrap().metadata.title().map(str::to_owned)
            };
            assert_eq!(title.as_deref(), Some("Pipeline title"), "{ext}");
            let output = crate::probe::probe(&dest).unwrap();
            assert!(
                output.codec.to_lowercase().contains(codec),
                "{}",
                output.codec
            );
            assert!((output.duration - 0.5).abs() < 0.06);
            let indexed = crate::media_io::Input::open(&dest).unwrap();
            let vi = indexed.video().unwrap();
            assert_eq!(indexed.tracks[vi].samples.len(), 12);
            assert!(
                indexed
                    .tracks
                    .iter()
                    .any(|t| t.kind == crate::media_io::Kind::Audio)
            );
            let source = crate::fc::open(&dest).unwrap();
            let decoded = source
                .video_frame(filmcraft_media::FrameRequest::full(
                    filmcraft_time::Tick::from_seconds_f64(0.2),
                ))
                .unwrap()
                .to_rgba8();
            assert!(
                decoded[0] > 200 && decoded[1] < 40 && decoded[2] < 40,
                "decoded output must contain processed red pixels"
            );
            let _ = std::fs::remove_file(dest);
        }
        // The default export preserves coded packets without processing,
        // including a container change, rather than silently encoding HEVC.
        let original = crate::media_io::Input::open(&input).unwrap();
        let vi = original.video().unwrap();
        for ext in ["mp4", "mkv"] {
            let dest = dir.join(format!("copied.{ext}"));
            crate::media_io::pipeline_export(
                &info, &dest, &[(0., 0.5)], &registry, &[], false, None,
                &Cancellation::default(), &Mutex::default(), |_| panic!("packet copy must not render pixels"),
            ).unwrap();
            let copied = crate::media_io::Input::open(&dest).unwrap();
            let cv = copied.video().unwrap();
            assert_eq!(copied.tracks[cv].samples.len(), original.tracks[vi].samples.len());
            for i in 0..original.tracks[vi].samples.len() {
                assert_eq!(original.data(vi, i).unwrap(), copied.data(cv, i).unwrap());
            }
            std::fs::remove_file(dest).unwrap();
        }
        for double in [false, true] {
            let dest = dir.join(if double { "double.mp4" } else { "cropped.mkv" });
            let keeps = if double {
                vec![(0., 0.5)]
            } else {
                vec![(0., 0.125), (0.25, 0.5)]
            };
            crate::media_io::pipeline_export(
                &info,
                &dest,
                &keeps,
                &registry,
                &[("test.red".into(), serde_json::json!({"double":double}))],
                !double,
                if double { None } else { Some((0, 0, 64, 64)) },
                &Cancellation::default(),
                &Mutex::default(),
                |_| Ok(()),
            )
            .unwrap();
            let output = crate::probe::probe(&dest).unwrap();
            assert!((output.fps - if double { 48. } else { 24. }).abs() < 0.01);
            assert_eq!(
                (output.width, output.height),
                if double { (320, 180) } else { (64, 64) }
            );
            let indexed = crate::media_io::Input::open(&dest).unwrap();
            assert_eq!(
                indexed.tracks[indexed.video().unwrap()].samples.len(),
                if double { 24 } else { 9 }
            );
            std::fs::remove_file(dest).unwrap();
        }
        for (generator, message) in [(true, "generator"), (false, "whole-clip")] {
            let mut unsupported = Registry::default();
            unsupported.register(Box::new(Unsupported(generator))).unwrap();
            let dest = dir.join("unsupported.mp4");
            let error = crate::media_io::pipeline_export(
                &info, &dest, &[(0., 0.5)], &unsupported,
                &[("test.unsupported".into(), serde_json::json!({}))], false, None,
                &Cancellation::default(), &Mutex::default(), |_| Ok(()),
            ).unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
            assert!(!dest.exists());
        }
        let cancelled = Cancellation::default();
        cancelled.cancel();
        let dest = dir.join("cancelled.mp4");
        assert!(
            crate::media_io::pipeline_export(
                &info,
                &dest,
                &[(0., 0.5)],
                &registry,
                &[],
                false,
                None,
                &cancelled,
                &Mutex::default(),
                |_| Ok(())
            )
            .is_err()
        );
        assert!(!dest.exists());
        let _ = std::fs::remove_dir_all(dir);
    }
}
