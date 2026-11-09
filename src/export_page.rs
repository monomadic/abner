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
    EditFilename,
    Destination,
    RevealDirectory,
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
    filename_edit: Option<String>,
    filename_selected: bool,
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
            filename_edit: None,
            filename_selected: false,
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
        let side = crate::app::rail_full_width(vp.0);
        let footer_y = vp.1 - crate::app::CUT_STATUS_H;
        let sidebar = RectPx { x: 0., y: crate::app::HEADER_H,
            w: side, h: footer_y - crate::app::HEADER_H };
        let context = RectPx { x: side, y: sidebar.y, w: vp.0 - side, h: 40. };
        let transport = RectPx { x: side, y: footer_y - 60., w: vp.0 - side, h: 60. };
        let video = RectPx { x: side, y: context.y + context.h,
            w: context.w, h: transport.y - context.y - context.h };
        let seek = RectPx { x: transport.x + 20., y: transport.y + 29.,
            w: (transport.w - 212.).max(80.), h: 4. };
        let progress = RectPx { x: side + 20., y: footer_y - 8., w: vp.0 - side - 40., h: 4. };

        Self {
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
                h: crate::app::CUT_STATUS_H,
            },
        }
    }
}
fn hit(r: RectPx, x: f32, y: f32) -> bool {
    x >= r.x && y >= r.y && x < r.x + r.w && y < r.y + r.h
}
impl Page {
    fn settings_geometry(l: Layout) -> (f32, f32, f32, f32) {
        (16., l.sidebar.w - 32., l.sidebar.y + 54., 28.)
    }
    pub fn editing_filename(&self) -> bool { self.open && self.filename_edit.is_some() }
    pub fn finish_filename(&mut self) -> bool {
        let Some(name) = self.filename_edit.as_ref() else { return true; };
        if name.trim().is_empty() || matches!(name.as_str(), "." | "..") || name.contains(['/', '\0', ':']) {
            self.status = "Enter a filename without path separators.".into();
            return false;
        }
        let Some(dir) = self.directory() else { return false; };
        let stem = name.strip_suffix(".mp4").or_else(|| name.strip_suffix(".mkv")).unwrap_or(name);
        let ext = if self.mkv { "mkv" } else { "mp4" };
        self.destination = Some(dir.join(format!("{stem}.{ext}")));
        self.filename_edit = None;
        self.filename_selected = false;
        true
    }
    pub fn text_input(&mut self, text: &str) -> bool {
        if !self.editing_filename() { return false; }
        let value = self.filename_edit.as_mut().unwrap();
        if self.filename_selected { value.clear(); self.filename_selected = false; }
        value.extend(text.chars().filter(|c| !c.is_control() && !matches!(c, '/' | ':')));
        true
    }
    pub fn shortcut(&mut self, c: char) -> bool {
        if !self.editing_filename() { return false; }
        match c.to_ascii_lowercase() {
            'a' => self.filename_selected = true,
            'v' => { if let Some(text) = crate::open::paste_text() { self.text_input(&text); } }
            _ => {}
        }
        true
    }
    fn directory(&self) -> Option<PathBuf> {
        self.destination.as_ref().or(self.source.as_ref())
            .and_then(|p| p.parent()).map(|p| p.to_path_buf())
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
            self.filename_edit = None;
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
        if !self.finish_filename() { return; }
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
                        .map(|t| t.chapters)
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
        if self.editing_filename() {
            match k {
                Key::Escape => { self.filename_edit = None; self.filename_selected = false; }
                Key::Enter | Key::Tab => { self.finish_filename(); }
                Key::Backspace => {
                    let value = self.filename_edit.as_mut().unwrap();
                    if self.filename_selected { value.clear(); self.filename_selected = false; } else { value.pop(); }
                }
                Key::Char(c) => { self.text_input(&c.to_string()); }
                Key::Space => { self.text_input(" "); }
                _ => {}
            }
            return true;
        }
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
        let (x, w, y, _) = Self::settings_geometry(l);
        let third = (w - 8.) / 3.;
        let half = (w - 4.) / 2.;
        let r = |x, y, w| RectPx { x, y, w, h: 26. };
        let mut actions = vec![
            (r(x, y + 16., third), "Copy", self.lossless, Action::Lossless),
            (r(x + third + 4., y + 16., third), "HEVC", !self.prores && !self.lossless, Action::Preset(false)),
            (r(x + 2. * (third + 4.), y + 16., third), "ProRes", self.prores && !self.lossless, Action::Preset(true)),
            (r(x, y + 76., half), "MKV", self.mkv, Action::Container(true)),
            (r(x + half + 4., y + 76., half), "MP4", !self.mkv, Action::Container(false)),
            (r(x, y + 136., w - 32.), "", self.filename_edit.is_some(), Action::EditFilename),
            (r(x + w - 26., y + 136., 26.), "Save as", false, Action::Destination),
            (r(x + w - 24., y + 196., 24.), "", false, Action::RevealDirectory),
            (r(l.context.x + 12., l.context.y + 7., 124.), "Original / Output", self.original, Action::Compare),
            (r(l.context.x + 144., l.context.y + 7., 112.), "Follow render", self.following, Action::Follow),
            (r(vp.0 - 124., l.footer.y + 6., 108.), "Export", false, Action::Export),
        ];
        if self.busy() {
            actions.push((r(x, l.footer.y - 36., w), "Cancel export", false, Action::Cancel));
        } else if self.path.is_some() {
            actions.push((r(x, l.footer.y - 36., w), "Reveal output", false, Action::Reveal));
        }
        actions
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
        if a != Action::EditFilename && !self.finish_filename() { return None; }
        match a {
            Action::EditFilename if !self.busy() => {
                if self.filename_edit.is_none() {
                    self.filename_edit = Some(self.destination_name());
                    self.filename_selected = false;
                }
            }
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
            Action::RevealDirectory => return self.directory(),
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
        use crate::app::{WORKSPACE_PANEL, WORKSPACE_TEXT, WORKSPACE_DIM, ACCENT};
        use crate::render::{TextItem, VAlign};
        let l = Layout::new(vp);
        let ink = WORKSPACE_TEXT;
        let dim = WORKSPACE_DIM;
        let mono = |items: &mut Vec<Item>, x, y, size, color, value: String, align, width| {
            items.push(Item::Text(TextItem { regular: true, valign: VAlign::Middle,
                align, max_width: Some(width), ..TextItem::new(x, y, size, color, value) }));
        };
        items.push(Item::Rect(RectItem::new(RectPx { x: 0., y: l.sidebar.y,
            w: vp.0, h: vp.1 - l.sidebar.y }, WORKSPACE_PANEL)));
        items.push(Item::Rect(RectItem::new(l.video, hex_color(0x1d1b1b))));
        let current = self.current();
        let fps = current.map_or(info.fps, |f| 1. / f.frame_duration);
        let dims = current.filter(|_| !self.original)
            .map_or((info.width, info.height), |f| (f.dims[0], f.dims[1]));
        let idx = if current.is_some() && !self.original { slot } else { source };
        let inset = if vp.0 < 900. { 16. } else { 24. };
        let picture = crate::app::App::fit_rect(dims, RectPx {
            x: l.video.x + inset, y: l.video.y + inset,
            w: l.video.w - inset * 2., h: l.video.h - inset * 2. });
        items.push(Item::Clip(Some(l.video)));
        if self.original && current.is_some_and(|f| (f.source_pts - shown_pts).abs() > 0.51 / info.fps.max(1.)) {
            ui_label(items, l.video.x + l.video.w / 2., l.video.y + l.video.h / 2.,
                12., dim, "Seeking matching original…", Align::Center, l.video.w);
        } else {
            items.push(Item::Video { a: idx, b: idx, r: picture,
                uv: [0., 0., 1., 1.], mode: VideoMode::Tex, p0: 0., p1: 0. });
        }
        items.push(Item::Clip(None));
        for r in [
            RectPx { x: l.sidebar.w - 1., y: l.sidebar.y, w: 1., h: l.sidebar.h },
            RectPx { x: 0., y: l.footer.y, w: vp.0, h: 1. },
        ] { items.push(Item::Rect(RectItem::new(r, hex_color(0x191919)))); }
        items.push(Item::Text(TextItem { header: true, valign: VAlign::Middle,
            max_width: Some(l.sidebar.w - 32.),
            ..TextItem::new(14., l.sidebar.y + 18., 13., ink, "Settings") }));
        let (x, w, y, _) = Self::settings_geometry(l);
        for (offset, title) in [(0., "Codec"), (60., "Container"), (120., "Filename"), (180., "Directory")] {
            mono(items, x, y + offset, 11., dim, title.into(), Align::Left, w);
        }
        mono(items, x, y + 209., 10., ink,
            self.directory().map(|p| p.display().to_string()).unwrap_or_default(),
            Align::Left, w - 34.);
        let summary_y = (l.footer.y - 146.).max(y + 250.);
        items.push(Item::Rect(RectItem::new(RectPx { x: 0., y: summary_y - 18.,
            w: l.sidebar.w - 1., h: l.footer.y - summary_y + 18. }, hex_color(0x0f0f0f))));
        let output_dims = current.map(|f| (f.dims[0], f.dims[1]))
            .or_else(|| self.crop.map(|(_, _, w, h)| (w, h))).unwrap_or((info.width, info.height));
        let codec = if self.lossless { info.codec.as_str() } else if self.prores { "ProRes" } else { "HEVC" };
        for (i, (key, value)) in [
            ("Video", codec.to_string()),
            ("Resolution", format!("{}×{}", output_dims.0, output_dims.1)),
            ("Framerate", format!("{fps:.3} fps")),
            ("Duration", format!("{:02}:{:02}", self.duration as u64 / 60, self.duration as u64 % 60)),
        ].into_iter().enumerate() {
            let yy = summary_y + i as f32 * 23.;
            mono(items, x, yy, 11., dim, key.into(), Align::Left, 78.);
            mono(items, x + w, yy, 11., ink, value, Align::Right, w - 82.);
        }
        // One transport row, with the same height, track and type as Sources.
        let time = current.map_or(0., |f| (f.output_time - f.frame_duration).max(0.));
        let fraction = if self.frames.len() > 1 { self.selected as f32 / (self.frames.len() - 1) as f32 } else { 0. };
        items.push(Item::Rect(RectItem { radius: 2., ..RectItem::new(l.seek, hex_color(0x454545)) }));
        if fraction > 0. {
            items.push(Item::Rect(RectItem { radius: 2., ..RectItem::new(RectPx { w: l.seek.w * fraction, ..l.seek }, ACCENT) }));
        }
        items.push(Item::Rect(RectItem { radius: 6., ..RectItem::new(RectPx {
            x: l.seek.x + l.seek.w * fraction - 6., y: l.seek.y - 4., w: 12., h: 12. }, ACCENT) }));
        ui_label(items, l.seek.x + l.seek.w + 18., l.transport.y + 30., 11., ink,
            format!("{:02}:{:06.3} / {:02}:{:06.3}", time as u64 / 60, time % 60., self.duration as u64 / 60, self.duration % 60.), Align::Left, 166.);
        if self.busy() {
            let done = (self.rendered_time / self.duration.max(0.001)).clamp(0., 1.);
            items.push(Item::Rect(RectItem::new(l.progress, hex_color(0x292929))));
            items.push(Item::Rect(RectItem::new(RectPx { w: l.progress.w * done as f32, ..l.progress }, ACCENT)));

        }
        ui_label(items, 16., l.footer.y + 19., 10., ACCENT, "EXPORT", Align::Left, 60.);
        let render_status = if self.busy() {
            let elapsed = self.started.elapsed().as_secs_f64().max(0.001);
            let done = (self.rendered_time / self.duration.max(0.001)).clamp(0., 1.);
            let eta = if done > 0. { format!(" · ~{:.0}s left", elapsed * (1. - done) / done) } else { String::new() };
            format!("{:.0}% · {} frames · {:.1} fps{eta}", done * 100., self.count, self.count as f64 / elapsed)
        } else { self.status.clone() };
        let status = if self.actions(vp).iter().any(|(r, _, _, a)| *a == Action::Lossless && hit(*r, cursor.0, cursor.1)) {
            self.copy_unavailable.unwrap_or(&self.status)
        } else { &render_status };
        ui_label(items, 98., l.footer.y + 19., 10., dim,
            status, Align::Left, (vp.0 - 238.).max(0.));
        for (r, t, selected, a) in self.actions(vp) {
            let hovered = hit(r, cursor.0, cursor.1);
            let disabled = match a {
                Action::Export => self.busy(),
                Action::Cancel => !self.busy(),
                Action::Reveal => self.path.is_none(),
                Action::Lossless => self.busy() || self.copy_unavailable.is_some(),
                Action::Container(false) => self.busy() || (self.prores && !self.lossless),
                Action::Preset(_) | Action::Container(true) | Action::Destination | Action::EditFilename => self.busy(),
                _ => false,
            };
            if a == Action::Destination {
                let color = if disabled { dim } else if hovered { ACCENT } else { ink };
                let disk = RectPx { x: r.x + 6., y: r.y + 6., w: 14., h: 14. };
                items.push(Item::Rect(RectItem { radius: 1., border_w: 1.2, border_color: color,
                    ..RectItem::new(disk, [0.; 4]) }));
                for mark in [
                    RectPx { x: disk.x + 3., y: disk.y, w: 8., h: 5. },
                    RectPx { x: disk.x + 3., y: disk.y + 9., w: 8., h: 5. },
                ] { items.push(Item::Rect(RectItem { border_w: 1., border_color: color, ..RectItem::new(mark, [0.; 4]) })); }
                continue;
            }
            if a == Action::RevealDirectory {
                let color = if hovered { ACCENT } else { hex_color(crate::app::INSPECTOR_PATH) };
                let folder = RectPx { x: r.x + 5., y: r.y + 9., w: 14., h: 10. };
                items.push(Item::Rect(RectItem { radius: 1., border_w: 1.3, border_color: color,
                    ..RectItem::new(folder, [0.; 4]) }));
                items.push(Item::Rect(RectItem { radius: 0.5, ..RectItem::new(
                    RectPx { x: folder.x, y: folder.y - 2.5, w: 6., h: 2.5 }, color) }));
                continue;
            }
            if r.y >= l.footer.y {
                crate::app::draw_cut_footer_button(items, r, t, "", false, hovered, disabled);
                continue;
            }
            let toolbar = matches!(a, Action::Compare | Action::Follow);
            items.push(Item::Rect(RectItem { radius: 6.,
                border_w: if toolbar { 0. } else { 1. },
                border_color: hex_color(if disabled { 0x252525 } else if selected { 0x315576 } else if hovered { 0x555555 } else { 0x2d2d2d }),
                ..RectItem::new(r, hex_color(if selected { if toolbar { 0x1a1a1a } else { 0x0b1926 } } else if toolbar { 0x050506 } else { 0x10151a })) }));
            let title = if a == Action::EditFilename {
                self.filename_edit.clone().unwrap_or_else(|| self.destination_name())
            } else { t.to_string() };
            if a == Action::EditFilename {
                items.push(Item::Text(TextItem { regular: true, valign: VAlign::Middle,
                    caret: self.filename_edit.is_some() && !self.filename_selected,
                    max_width: Some(r.w - 16.),
                    color: if self.filename_selected { ACCENT } else { ink },
                    ..TextItem::new(r.x + 8., r.y + r.h / 2., 10., if disabled { dim } else { ink }, title) }));
            } else {
                ui_label(items, r.x + r.w / 2., r.y + r.h / 2., 11.,
                    if disabled { hex_color(0x686868) } else if selected || hovered { ink } else { dim },
                    title, Align::Center, r.w - 12.);
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
        let actions = p.actions((720., 480.));
        let cancel = actions.iter().find(|(.., a)| *a == Action::Cancel).unwrap();
        assert!(cancel.0.y < Layout::new((720., 480.)).footer.y);
        assert!(!actions.iter().any(|(.., a)| *a == Action::Reveal));
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
        assert!(!p.actions((720., 480.)).iter().any(|(.., a)| matches!(a, Action::Cancel | Action::Reveal)));
        for vp in [(720., 480.), (900., 600.), (1280., 800.), (1440., 900.)] {
            let l = Layout::new(vp);
            assert_eq!(l.sidebar.x, 0.);
            assert_eq!(l.video.x, l.sidebar.w);
            assert_eq!(l.video.y, crate::app::HEADER_H + 40.);
            assert_eq!(l.video.w + l.sidebar.w, vp.0);
            assert_eq!(l.sidebar.y, crate::app::HEADER_H);
            assert_eq!(p.actions(vp).iter().filter(|(.., a)| *a == Action::Export).count(), 1);
            assert!(l.progress.y + 7. < l.footer.y);
            for (r, _, _, a) in p.actions(vp) {
                assert!(r.x >= 0. && r.x + r.w <= vp.0 + 0.1);
                assert!(r.y >= crate::app::HEADER_H && r.y + r.h <= vp.1);
                assert!(!hit(l.video, r.x, r.y));
                if matches!(a, Action::Compare | Action::Follow) {
                    assert!(r.y >= l.context.y && r.y + r.h <= l.context.y + l.context.h);
                }
                if r.y >= l.footer.y {
                    assert!(matches!(
                        a,
                        Action::Export
                    ));
                }
            }
        }
    }
    #[test]
    fn directory_reveal_uses_destination_parent_and_completion_reveal_stays_in_panel() {
        let mut p = Page::default();
        p.source = Some(PathBuf::from("/clips/source.mp4"));
        assert_eq!(p.apply(Action::RevealDirectory), Some(PathBuf::from("/clips")));
        p.destination = Some(PathBuf::from("/exports/output.mp4"));
        assert_eq!(p.apply(Action::RevealDirectory), Some(PathBuf::from("/exports")));
        p.path = p.destination.clone();
        let actions = p.actions((720., 480.));
        let reveal = actions.iter().find(|(.., a)| *a == Action::Reveal).unwrap();
        assert!(reveal.0.y < Layout::new((720., 480.)).footer.y);
        assert_eq!(p.apply(Action::Reveal), p.destination);
    }
    #[test]
    fn filename_edits_commit_in_place_without_changing_directory() {
        let mut p = Page::default();
        p.open = true;
        p.source = Some(PathBuf::from("/clips/source.mp4"));
        p.apply(Action::EditFilename);
        p.text_input(" extra");
        assert_eq!(p.filename_edit.as_deref(), Some("source.export.mp4 extra"));
        p.shortcut('a');
        p.text_input("My render.mp4");
        p.key(Key::Enter);
        assert_eq!(p.destination, Some(PathBuf::from("/clips/My render.mp4")));
        p.apply(Action::EditFilename);
        p.shortcut('a');
        p.text_input("discard");
        p.key(Key::Escape);
        assert_eq!(p.destination_name(), "My render.mp4");
        p.apply(Action::EditFilename);
        p.shortcut('a');
        p.key(Key::Backspace);
        assert!(!p.finish_filename());
        p.text_input("final");
        p.key(Key::Tab);
        assert_eq!(p.destination_name(), "final.mp4");
        let actions = p.actions((720., 480.));
        let field = actions.iter().find(|(.., a)| *a == Action::EditFilename).unwrap().0;
        let save_as = actions.iter().find(|(.., a)| *a == Action::Destination).unwrap().0;
        assert!(save_as.x >= field.x + field.w);
        assert_eq!(field.y, save_as.y);
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
                &[crate::media_io::ExportChapter::new(0., 0.25, "One"), crate::media_io::ExportChapter::new(0.25, 0.5, "Two")],
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
            assert_eq!(chapters.iter().map(|c| (c.start, c.title.as_str())).collect::<Vec<_>>(), [(0., "One"), (0.25, "Two")], "{ext}");
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
