//! Minimal export workspace. Workers publish a bounded latest-frame mailbox; no GUI waits for rendering.
use crate::canvas::{Canvas, ButtonBar, TimelineAttachment};
use crate::{
    app::{Key, hex_color, ui_label},
    player::Rgba,
    probe::VideoInfo,
    render::{Align, Item, RectItem, RectPx, Upload, VideoMode},
};
use crate::text_edit::{Cursor, Edit};
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
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Container {
    Mp4,
    Mov,
    Mkv,
}

impl Container {
    fn extension(self) -> &'static str {
        match self {
            Self::Mp4 => "mp4",
            Self::Mov => "mov",
            Self::Mkv => "mkv",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Action {
    Preset(bool),
    Lossless,
    Container(Container),
    Export,
    EditFilename,
    Destination,
    RevealDirectory,
    Cancel,
    Compare,
    Follow,
    Play,
    Step(bool),
    Seek(f64),
    Reveal,
}
/// A stage's in-flight report during an export, with the stage's name.
pub(crate) type StageReport = Arc<dyn Fn(&str, abner_processor_api::Progress) + Send + Sync>;
/// `90` → `1m 30s`; under a minute, whole seconds.
fn remaining(seconds: f64) -> String {
    let s = seconds.ceil().max(0.) as u64;
    if s >= 3600 { format!("{}h {:02}m", s / 3600, s / 60 % 60) }
    else if s >= 60 { format!("{}m {:02}s", s / 60, s % 60) }
    else { format!("{s}s") }
}
/// The footer line of a running render: the whole clip first (share done,
/// frames out of the total once a frame has given the output rate, pace, time
/// left at that pace), then what the working stage says it is doing. Before
/// the first frame only the stage has anything to report.
fn render_line(done: f64, count: u64, frame_duration: Option<f64>, duration: f64, elapsed: f64,
    stage: Option<&(String, abner_processor_api::Progress)>) -> String {
    let frames = match frame_duration.filter(|d| *d > 0.) {
        Some(d) => format!("{count} of {:.0} frames", (duration / d).round().max(count as f64)),
        None => format!("{count} frames"),
    };
    let pace = count as f64 / elapsed.max(0.001);
    // A model that takes minutes a frame reads as "0.0 fps".
    let pace = if count == 0 { String::new() }
        else if pace >= 1. { format!(" · {pace:.1} fps") }
        else { format!(" · {} per frame", remaining(1. / pace)) };
    let eta = if done > 0. && done < 1. { format!(" · ~{} left", remaining(elapsed * (1. - done) / done)) } else { String::new() };
    let stage = stage.map_or_else(String::new, |(name, p)| {
        let phase = if p.phase.trim().is_empty() { "Processing" } else { &p.phase };
        match p.total.filter(|t| *t > 0) {
            Some(t) => format!(" · {name} · {phase} {:.0}%", (p.completed.min(t) as f64 / t as f64 * 100.).floor()),
            None => format!(" · {name} · {phase}"),
        }
    });
    format!("{:.0}% · {frames}{pace}{eta}{stage}", (done * 100.).floor())
}
struct Job {
    latest: Arc<Mutex<Option<Rendered>>>,
    stage: Arc<Mutex<Option<(String, abner_processor_api::Progress)>>>,
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
    container: Container,
    pub following: bool,
    pub original: bool,
    job: Option<Job>,
    frames: VecDeque<Rendered>,
    selected: usize,
    playback: Option<(Instant, f64)>,
    held: Option<Rendered>,
    started: Instant,
    elapsed: f64,
    pub status: String,
    destination: Option<PathBuf>,
    filename_edit: Option<String>,
    filename_cursor: Cursor,
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
            container: Container::Mp4,
            following: true,
            original: false,
            job: None,
            frames: VecDeque::new(),
            selected: 0,
            playback: None,
            held: None,
            started: Instant::now(),
            elapsed: 0.,
            status: "Choose a preset and export the enabled pipeline.".into(),
            destination: None,
            filename_edit: None,
            filename_cursor: Cursor::default(),
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
    pub canvas: Canvas,
    pub context: RectPx,
    pub transport: RectPx,
    pub footer: RectPx,
    pub seek: RectPx,
}
impl Layout {
    pub fn new(vp: (f32, f32)) -> Self {
        let side = crate::app::rail_full_width(vp.0);
        let footer_y = vp.1 - crate::app::CUT_STATUS_H;
        let sidebar = RectPx { x: 0., y: crate::app::HEADER_H,
            w: side, h: footer_y - crate::app::HEADER_H };
        let canvas = Canvas::layout(RectPx{x:side,y:sidebar.y,w:vp.0-side,h:footer_y-sidebar.y},Some(40.),
            Some(TimelineAttachment{x:side,width:vp.0-side,transport_height:60.,lanes_height:0.}));
        let context=canvas.button_bar.unwrap().r;
        let transport=canvas.timeline.unwrap().transport;
        let video=canvas.r;
        let seek = RectPx { x: transport.x + 20., y: transport.y + 15.,
            w: (transport.w - 212.).max(80.), h: 4. };

        Self {
            sidebar,
            video,
            canvas,
            context,
            transport,
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
        let stem = name.rsplit_once('.')
            .filter(|(_, ext)| ["mp4", "mov", "mkv"].iter().any(|known| ext.eq_ignore_ascii_case(known)))
            .map_or(name.as_str(), |(stem, _)| stem);
        let ext = self.container.extension();
        self.destination = Some(dir.join(format!("{stem}.{ext}")));
        self.filename_edit = None;
        self.filename_cursor = Cursor::default();
        true
    }
    pub fn text_input(&mut self, text: &str) -> bool {
        if !self.editing_filename() { return false; }
        let value = self.filename_edit.as_mut().unwrap();
        let filtered: String = text.chars().filter(|c| !c.is_control() && !matches!(c, '/' | ':')).collect();
        self.filename_cursor.insert(value, &filtered, usize::MAX);
        true
    }
    pub(crate) fn edit_text(&mut self, edit: Edit) -> bool {
        if !self.editing_filename() { return false; }
        if matches!(edit, Edit::Paste) {
            if let Some(text) = crate::open::paste_text() { self.text_input(&text); }
        } else {
            self.filename_cursor.apply(self.filename_edit.as_mut().unwrap(), edit);
        }
        true
    }
    fn directory(&self) -> Option<PathBuf> {
        self.destination.as_ref().or(self.source.as_ref())
            .and_then(|p| p.parent()).map(|p| p.to_path_buf())
    }
    fn destination_name(&self) -> String {
        let ext = self.container.extension();
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
            self.playback = None;
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
        if !self.lossless && self.prores && self.container == Container::Mp4 {
            self.status = "ProRes requires MOV or MKV.".into();
            return;
        }
        if !self.finish_filename() { return; }
        let ext = self.container.extension();
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
        self.playback = None;
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
        let stage = Arc::new(Mutex::new(None));
        let report: StageReport = {
            let stage = stage.clone();
            Arc::new(move |name, p| *stage.lock().unwrap() = Some((name.to_owned(), p)))
        };
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
                    Some(&report),
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
            stage,
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
        if !self.open {
            self.playback = None;
        }
        if let Some((started, position)) = self.playback {
            let time = position + started.elapsed().as_secs_f64();
            if let Some(last) = self.frames.back() {
                let finished = time >= last.output_time;
                let index = self.frame_at(time);
                if self.current().is_none_or(|f| f.count != self.frames[index].count) {
                    self.show_frame(index);
                }
                if finished { self.playback = None; }
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
    pub fn playing(&self) -> bool {
        self.open && self.playback.is_some()
    }

    fn frame_time(frame: &Rendered) -> f64 {
        (frame.output_time - frame.frame_duration).max(0.)
    }

    fn frame_at(&self, time: f64) -> usize {
        self.frames.partition_point(|frame| Self::frame_time(frame) <= time)
            .saturating_sub(1)
    }

    fn show_frame(&mut self, index: usize) {
        self.selected = index;
        if let Some(frame) = self.frames.get(index) {
            self.held = Some(frame.clone());
            self.seek = Some(frame.source_pts);
            self.dirty = true;
        }
    }

    fn toggle_playback(&mut self) {
        if self.playback.take().is_some() || self.frames.is_empty() { return; }
        let last = self.frames.back().unwrap();
        let index = match self.current() {
            Some(frame) if frame.count < last.count => self.frame_at(Self::frame_time(frame)),
            _ => 0,
        };
        self.following = false;
        self.show_frame(index);
        self.playback = Some((Instant::now(), Self::frame_time(&self.frames[index])));
    }

    fn step_preview(&mut self, forward: bool) {
        let Some(frame) = self.current() else { return; };
        let index = self.frames.partition_point(|f| f.count < frame.count);
        let cached = self.frames.get(index).is_some_and(|f| f.count == frame.count);
        let index = if forward {
            (index + usize::from(cached)).min(self.frames.len() - 1)
        } else {
            index.saturating_sub(1)
        };
        self.following = false;
        self.playback = None;
        self.show_frame(index);
    }

    pub fn enter(&mut self) {
        self.open = true;
        self.dirty = true;
    }
    pub fn key(&mut self, k: Key) -> bool {
        if self.editing_filename() {
            match k {
                Key::Escape => { self.filename_edit = None; self.filename_cursor = Cursor::default(); }
                Key::Enter | Key::Tab => { self.finish_filename(); }
                Key::Backspace => { self.edit_text(Edit::Backspace); }
                Key::Left => { self.edit_text(Edit::Left); }
                Key::Right => { self.edit_text(Edit::Right); }
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
            Key::Space => { self.toggle_playback(); true }
            Key::Left | Key::Right => { self.step_preview(k == Key::Right); true }
            _ => false,
        }
    }
    pub fn actions(&self, vp: (f32, f32)) -> Vec<(RectPx, &'static str, bool, Action)> {
        let l = Layout::new(vp);
        let (x, w, y, _) = Self::settings_geometry(l);
        let third = (w - 8.) / 3.;
        let r = |x, y, w| RectPx { x, y, w, h: 26. };
        let mut actions = vec![
            (r(x, y + 16., third), "Copy", self.lossless, Action::Lossless),
            (r(x + third + 4., y + 16., third), "HEVC", !self.prores && !self.lossless, Action::Preset(false)),
            (r(x + 2. * (third + 4.), y + 16., third), "ProRes", self.prores && !self.lossless, Action::Preset(true)),
            (r(x, y + 76., third), "MP4", self.container == Container::Mp4, Action::Container(Container::Mp4)),
            (r(x + third + 4., y + 76., third), "MOV", self.container == Container::Mov, Action::Container(Container::Mov)),
            (r(x + 2. * (third + 4.), y + 76., third), "MKV", self.container == Container::Mkv, Action::Container(Container::Mkv)),
            (r(x, y + 136., w - 32.), "", self.filename_edit.is_some(), Action::EditFilename),
            (r(x + w - 26., y + 136., 26.), "Save as", false, Action::Destination),
            (r(x + w - 24., y + 196., 24.), "", false, Action::RevealDirectory),
            (r(l.context.x + 12., l.context.y + 7., 124.), "Original / Output", self.original, Action::Compare),
            (r(l.transport.x + 20., l.transport.y + 30., 26.), "‹", false, Action::Step(false)),
            (r(l.transport.x + 50., l.transport.y + 30., 56.), if self.playing() { "Pause" } else { "Play" }, false, Action::Play),
            (r(l.transport.x + 110., l.transport.y + 30., 26.), "›", false, Action::Step(true)),
            (r(l.transport.x + l.transport.w - 134., l.transport.y + 30., 114.), "Follow render", self.following, Action::Follow),
        ];
        let primary = RectPx { x: vp.0 - 164., y: l.context.y + 4., w: 148., h: 32. };
        if self.busy() {
            actions.push((primary, "Cancel export", false, Action::Cancel));
        } else if self.path.is_some() {
            actions.push((r(primary.x - 94., l.context.y + 7., 86.), "Export", false, Action::Export));
            actions.push((primary, "Reveal output", false, Action::Reveal));
        } else {
            actions.push((primary, "Export", false, Action::Export));
        }
        actions
    }

    pub fn click(&self, vp: (f32, f32), x: f32, y: f32) -> Option<Action> {
        let l = Layout::new(vp);
        if hit(
            RectPx {
                y: l.seek.y - 12.,
                h: 26.,
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
                    self.filename_cursor = Cursor::default();
                }
            }
            Action::Preset(p) if !self.busy() => {
                self.lossless = false;
                self.prores = p;
                if p && self.container == Container::Mp4 {
                    self.container = Container::Mov;
                }
            }
            Action::Lossless if !self.busy() && self.copy_unavailable.is_none() => {
                self.lossless = true;
                self.prores = false;
            }
            Action::Container(container) if !self.busy() => {
                if container == Container::Mp4 && self.prores && !self.lossless {
                    self.status = "ProRes requires MOV or MKV.".into();
                } else {
                    self.container = container;
                }
            }
            Action::Destination if !self.busy() => {
                if let Some(mut path) = crate::open::export_destination(&self.destination_name()) {
                    path.set_extension(self.container.extension());
                    self.destination = Some(path);
                }
            }
            Action::Cancel => self.cancel(),
            Action::Compare => self.original = !self.original,
            Action::Play => self.toggle_playback(),
            Action::Step(forward) => self.step_preview(forward),
            Action::Follow => {
                self.playback = None;
                self.following = !self.following;
                self.show_frame(self.frames.len().saturating_sub(1));
            }
            Action::Seek(fraction) if !self.frames.is_empty() => {
                self.following = false;
                self.playback = None;
                self.show_frame(self.frame_at(fraction * self.duration));
            }
            Action::RevealDirectory => return self.directory(),
            Action::Reveal => return self.path.clone(),
            _ => {}
        }
        None
    }
    pub(crate) fn canvas(vp:(f32,f32),show_ui:bool) -> Canvas {
        if show_ui { Layout::new(vp).canvas } else { Canvas::new(RectPx{x:0.,y:0.,w:vp.0,h:vp.1}) }
    }
    pub(crate) fn display_dims(&self, info:&VideoInfo)->(u32,u32) {
        self.current().filter(|_| !self.original).map_or((info.width,info.height),|f|(f.dims[0],f.dims[1]))
    }
    pub(crate) fn base_rect(&self,vp:(f32,f32),show_ui:bool,info:&VideoInfo)->RectPx {
        Self::canvas(vp,show_ui).fit_inset(self.display_dims(info),if !show_ui {0.}else if vp.0<900. {16.}else{24.})
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
        view: (f32,(f32,f32)),
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
                    r: crate::canvas::zoomed(crate::canvas::fit_rect(dims, r),view.0,view.1),
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
        l.canvas.ground(items);
        if let Some(bar) = l.canvas.button_bar { bar.draw(items); }
        let current = self.current();
        let fps = current.map_or(info.fps, |f| 1. / f.frame_duration);
        let idx = if current.is_some() && !self.original { slot } else { source };
        let picture = crate::canvas::zoomed(self.base_rect(vp,show_ui,info),view.0,view.1);
        l.canvas.clip(items);
        Canvas::lift(items,picture);
        if self.original && current.is_some_and(|f| (f.source_pts - shown_pts).abs() > 0.51 / info.fps.max(1.)) {
            ui_label(items, l.video.x + l.video.w / 2., l.video.y + l.video.h / 2.,
                12., dim, "Seeking matching original…", Align::Center, l.video.w);
        } else {
            items.push(Item::Video { a: idx, b: idx, r: picture,
                uv: [0., 0., 1., 1.], mode: VideoMode::Tex, p0: 0., p1: 0. });
        }
        l.canvas.recess(items);
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
        let time = current.map_or(0., Self::frame_time);
        let fraction = |time: f64| (time / self.duration.max(0.001)).clamp(0., 1.) as f32;
        let position = fraction(time);
        items.push(Item::Rect(RectItem { radius: 2., ..RectItem::new(l.seek, hex_color(0x454545)) }));
        if let (Some(first), Some(last)) = (self.frames.front(), self.frames.back()) {
            let start = fraction(Self::frame_time(first));
            let end = fraction(last.output_time);
            items.push(Item::Rect(RectItem::new(RectPx {
                x: l.seek.x + l.seek.w * start, w: l.seek.w * (end - start), ..l.seek
            }, hex_color(0x265473))));
            items.push(Item::Rect(RectItem { radius: 6., ..RectItem::new(RectPx {
                x: l.seek.x + l.seek.w * position - 6., y: l.seek.y - 4., w: 12., h: 12.
            }, ACCENT) }));
        }
        ui_label(items, l.seek.x + l.seek.w + 18., l.seek.y + 2., 11., ink,
            format!("{:02}:{:06.3} / {:02}:{:06.3}", time as u64 / 60, time % 60., self.duration as u64 / 60, self.duration % 60.), Align::Left, 166.);
        let amber = hex_color(0xffb653);
        ui_label(items, 16., l.footer.y + if self.busy() { 12. } else { 19. }, 10.,
            if self.busy() { amber } else { dim }, "EXPORT", Align::Left, 60.);
        if self.busy() && !self.lossless {
            let done = fraction(self.rendered_time);
            for i in 0..10 {
                let cell = RectPx { x: 16. + i as f32 * 7., y: l.footer.y + 23., w: 5., h: 8. };
                items.push(Item::Rect(RectItem::new(cell, hex_color(0x372c1e))));
                let filled = (done * 10. - i as f32).clamp(0., 1.);
                if filled > 0. {
                    items.push(Item::Rect(RectItem::new(RectPx { h: cell.h * filled,
                        y: cell.y + cell.h * (1. - filled), ..cell }, amber)));
                }
            }
        }
        let render_status = if self.busy() && !self.lossless {
            let done = (self.rendered_time / self.duration.max(0.001)).clamp(0., 1.);
            let stage = self.job.as_ref().and_then(|j| j.stage.lock().unwrap().clone());
            render_line(done, self.count, self.frames.back().map(|f| f.frame_duration), self.duration,
                self.started.elapsed().as_secs_f64(), stage.as_ref())
        } else { self.status.clone() };
        let status = if self.actions(vp).iter().any(|(r, _, _, a)| *a == Action::Lossless && hit(*r, cursor.0, cursor.1)) {
            self.copy_unavailable.unwrap_or(&self.status)
        } else { &render_status };
        ui_label(items, 98., l.footer.y + 19., 10., dim,
            status, Align::Left, (vp.0 - 114.).max(0.));
        for (r, t, selected, a) in self.actions(vp) {
            let hovered = hit(r, cursor.0, cursor.1);
            let disabled = match a {
                Action::Export => self.busy(),
                Action::Cancel => !self.busy(),
                Action::Reveal => self.path.is_none(),
                Action::Play | Action::Step(_) => self.frames.is_empty(),
                Action::Lossless => self.busy() || self.copy_unavailable.is_some(),
                Action::Container(Container::Mp4) => self.busy() || (self.prores && !self.lossless),
                Action::Preset(_) | Action::Container(_) | Action::Destination | Action::EditFilename => self.busy(),
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
            if a == Action::Reveal || (a == Action::Export && self.path.is_none()) {
                ButtonBar::primary(items,r,t,hovered,disabled);
                continue;
            }
            if a == Action::Follow {
                let track = RectPx { x: r.x, y: r.y + 6., w: 28., h: 14. };
                items.push(Item::Rect(RectItem { radius: 7., ..RectItem::new(track,
                    if selected { ACCENT } else { hex_color(0x414141) }) }));
                items.push(Item::Rect(RectItem { radius: 5., ..RectItem::new(RectPx {
                    x: track.x + if selected { 16. } else { 2. }, y: track.y + 2., w: 10., h: 10.
                }, ink) }));
                ui_label(items, r.x + 34., r.y + 13., 11., if hovered || selected { ink } else { dim },
                    t, Align::Left, r.w - 34.);
                continue;
            }
            if r.y >= l.context.y && r.y + r.h <= l.context.y + l.context.h && r.x >= l.context.x {
                ButtonBar::button(items,r,t,selected,hovered,disabled);
                continue;
            }
            items.push(Item::Rect(RectItem { radius: 6.,
                border_w: 1.,
                border_color: hex_color(if disabled { 0x252525 } else if selected { 0x315576 } else if hovered { 0x555555 } else { 0x2d2d2d }),
                ..RectItem::new(r, hex_color(if selected { 0x0b1926 } else { 0x10151a })) }));
            let title = if a == Action::EditFilename {
                self.filename_edit.clone().unwrap_or_else(|| self.destination_name())
            } else { t.to_string() };
            if a == Action::EditFilename {
                items.push(Item::Text(TextItem { regular: true, valign: VAlign::Middle,
                    caret: self.filename_edit.as_ref().map(|name| self.filename_cursor.position(name)),
                    caret_visible: !self.filename_cursor.selected(),
                    max_width: Some(r.w - 16.),
                    color: if self.filename_cursor.selected() { ACCENT } else { ink },
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
        reporter: Option<abner_processor_api::ProgressReporter>,
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
                reporter: None,
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
        fn set_progress_reporter(&mut self, reporter: abner_processor_api::ProgressReporter) {
            self.reporter = Some(reporter);
        }
        fn process(&mut self, mut f: Frame) -> Result<Vec<Frame>, String> {
            if let Some(report) = &self.reporter {
                report(Progress { phase: "painting".into(), completed: 1, total: Some(2) });
            }
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
        assert_eq!(page.container, Container::Mov);
        page.apply(Action::Container(Container::Mkv));
        page.apply(Action::Preset(true));
        assert_eq!(page.container, Container::Mkv);
        page.apply(Action::Lossless);
        page.apply(Action::Container(Container::Mp4));
        assert_eq!(page.container, Container::Mp4, "the render preset must not restrict stream copy");
    }

    #[test]
    fn manual_review_stays_put_while_latest_progress_advances() {
        let mut p = Page::default();
        p.open = true;
        p.duration = 10.;
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
            stage: Arc::default(),
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
        p.apply(Action::Follow);
        assert!(!p.following);
        assert_eq!(p.current().unwrap().count, 2);
        p.apply(Action::Seek(0.05));
        assert_eq!(p.current().unwrap().count, 1, "seek uses output time");
        p.apply(Action::Play);
        assert!(p.playing());
        p.playback = Some((Instant::now(), 2.));
        p.poll(&mut uploads, 3);
        assert_eq!(p.current().unwrap().count, 2);
        assert!(!p.playing(), "playback stops at the cached frontier");
        p.key(Key::Tab);
        assert!(p.original);
    }
    #[test]
    fn render_line_leads_with_the_whole_clip_then_the_working_stage() {
        let stage = |phase: &str, completed, total| {
            ("Example".to_string(), Progress { phase: phase.into(), completed, total })
        };
        // Before the first frame only the stage has anything to say.
        assert_eq!(render_line(0., 0, None, 20., 90., Some(&stage("Window 1 · encoding", 9, Some(18)))),
            "0% · 0 frames · Example · Window 1 · encoding 50%");
        // 120 of 480 frames in ten minutes: five seconds a frame, thirty minutes to go.
        assert_eq!(render_line(0.25, 120, Some(1. / 24.), 20., 600., Some(&stage("Window 25 · restoring", 16, Some(32)))),
            "25% · 120 of 480 frames · 5s per frame · ~30m 00s left · Example · Window 25 · restoring 50%");
        assert_eq!(render_line(0.5, 240, Some(1. / 24.), 20., 10., None),
            "50% · 240 of 480 frames · 24.0 fps · ~10s left");
        assert_eq!(render_line(0.5, 1, Some(0.5), 1., 4000., Some(&stage(" ", 3, None))),
            "50% · 1 of 2 frames · 1h 06m per frame · ~1h 06m left · Example · Processing");
        // A rounded total never reads below the frames already written.
        assert_eq!(render_line(1., 481, Some(1. / 24.), 20., 10., None), "100% · 481 of 481 frames · 48.1 fps");
    }
    #[test]
    fn minimal_controls_fit_and_footer_is_status_only() {
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
            for (r, _, _, a) in p.actions(vp) {
                assert!(r.x >= 0. && r.x + r.w <= vp.0 + 0.1);
                assert!(r.y >= crate::app::HEADER_H && r.y + r.h <= vp.1);
                assert!(!hit(l.video, r.x, r.y));
                if a == Action::Compare {
                    assert!(r.y >= l.context.y && r.y + r.h <= l.context.y + l.context.h);
                }
                if matches!(a, Action::Follow | Action::Play | Action::Step(_)) {
                    assert!(r.y >= l.seek.y + 14. && r.y + r.h <= l.footer.y);
                    assert_eq!(p.click(vp, r.x + 2., r.y + 2.), Some(a));
                }
                assert!(r.y + r.h <= l.footer.y);
                if a == Action::Export {
                    assert!(r.y >= l.context.y && r.y + r.h <= l.context.y + l.context.h);
                    assert_eq!(r.x + r.w, vp.0 - 16.);
                }
            }
        }
    }
    #[test]
    fn directory_reveal_uses_destination_parent_and_output_reveal_is_top_right() {
        let mut p = Page::default();
        p.source = Some(PathBuf::from("/clips/source.mp4"));
        assert_eq!(p.apply(Action::RevealDirectory), Some(PathBuf::from("/clips")));
        p.destination = Some(PathBuf::from("/exports/output.mp4"));
        assert_eq!(p.apply(Action::RevealDirectory), Some(PathBuf::from("/exports")));
        p.path = p.destination.clone();
        let actions = p.actions((720., 480.));
        let reveal = actions.iter().find(|(.., a)| *a == Action::Reveal).unwrap();
        let l = Layout::new((720., 480.));
        assert!(reveal.0.y >= l.context.y && reveal.0.y + reveal.0.h <= l.video.y);
        assert_eq!(reveal.0.x + reveal.0.w, 704.);
        let export = actions.iter().find(|(.., a)| *a == Action::Export).unwrap();
        assert!(export.0.x + export.0.w < reveal.0.x);
        assert_eq!(p.click((720., 480.), reveal.0.x + 1., reveal.0.y + 1.), Some(Action::Reveal));
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
        p.edit_text(Edit::SelectAll);
        p.text_input("My render.mp4");
        p.key(Key::Enter);
        assert_eq!(p.destination, Some(PathBuf::from("/clips/My render.mp4")));
        p.apply(Action::EditFilename);
        p.edit_text(Edit::SelectAll);
        p.text_input("discard");
        p.key(Key::Escape);
        assert_eq!(p.destination_name(), "My render.mp4");
        p.apply(Action::EditFilename);
        p.edit_text(Edit::SelectAll);
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
        check_native_pipeline(false);
    }

    #[test]
    fn native_pipeline_writes_prores_and_copies_packets() {
        check_native_pipeline(true);
    }

    fn check_native_pipeline(prores_only: bool) {
        let _ = env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
            .is_test(true)
            .try_init();
        let dir = std::env::temp_dir().join(format!("abner-pipeline-test-{}-{prores_only}", std::process::id()));
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
            (false, "mov", "hevc"),
            (true, "mov", "prores"),
            (true, "mkv", "prores"),
        ].into_iter().filter(|(prores, _, _)| *prores == prores_only) {
            let dest = dir.join(format!("{}.{}", codec, ext));
            let _ = std::fs::remove_file(&dest);
            let mut previews = 0;
            let stages = Arc::new(Mutex::new(Vec::new()));
            let report: StageReport = {
                let stages = stages.clone();
                Arc::new(move |name, p| stages.lock().unwrap().push((name.to_owned(), p.phase, p.total)))
            };
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
                Some(&report),
                |f| {
                    previews += 1;
                    assert_eq!(&f.pixels[..4], &[255, 0, 0, 255]);
                    Ok(())
                },
            )
            .unwrap();
            assert_eq!(previews, 12);
            // The stage's own reports reach the page under the stage's name, one per input frame.
            let seen = stages.lock().unwrap();
            assert_eq!(seen.len(), 12);
            assert!(seen.iter().all(|(name, phase, total)| !name.is_empty() && phase == "painting" && *total == Some(2)));
            if ext == "mov" {
                let bytes = std::fs::read(&dest).unwrap();
                assert_eq!(&bytes[4..12], b"ftypqt  ");
            }
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
        for ext in ["mp4", "mov", "mkv"] {
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
        for double in [false, true].into_iter().filter(|_| !prores_only) {
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
