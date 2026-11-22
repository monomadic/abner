//! Minimal export workspace. Workers publish a bounded latest-frame mailbox; no GUI waits for rendering.
use crate::canvas::{Canvas, ButtonBar, TimelineAttachment};
use crate::{
    app::{Key, hex_color},
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
    OpenExport(usize),
    RevealDirectory,
    Cancel,
    Follow,
    Play,
    Step(bool),
    Keyframe(bool),
    ChooseDirectory,
    Seek(f64),
    Reveal,
}
/// A stage's in-flight report during an export, with the stage's name.
pub(crate) type StageReport = Arc<dyn Fn(&str, StageUpdate) + Send + Sync>;
pub(crate) enum StageUpdate {
    /// The stage's phase-local detail.
    Phase(abner_processor_api::Progress),
    /// Stage `.0` of `.1`'s current call (the frame bar); `None` once the
    /// call returns.
    Frame(usize, usize, Option<abner_processor_api::FrameProgress>),
}
/// The working stage's frame report: name, index, stage count, steps.
type FrameReport = (String, usize, usize, abner_processor_api::FrameProgress);
/// The frame bar's live report, and the total bar's high-water mark: a frame
/// report cleared just before its frame lands never moves the bar back.
#[derive(Default)]
struct FrameState {
    report: Option<FrameReport>,
    floor: f64,
}
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
    stage: Option<&(String, abner_processor_api::Progress)>, frame: Option<&FrameReport>) -> String {
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
    let stage = if let Some((name, _, _, f)) = frame {
        let phase = if f.phase.trim().is_empty() { "Processing" } else { &f.phase };
        format!(" · {name} · {phase} {}/{}", f.completed.min(f.total), f.total)
    } else { stage.map_or_else(String::new, |(name, p)| {
        let phase = if p.phase.trim().is_empty() { "Processing" } else { &p.phase };
        match p.total.filter(|t| *t > 0) {
            Some(t) => format!(" · {name} · {phase} {:.0}%", (p.completed.min(t) as f64 / t as f64 * 100.).floor()),
            None => format!(" · {name} · {phase}"),
        }
    }) };
    format!("{:.0}% · {frames}{pace}{eta}{stage}", (done * 100.).floor())
}
struct Job {
    launch: Option<Box<dyn FnOnce() + Send>>,
    latest: Arc<Mutex<Option<Rendered>>>,
    stage: Arc<Mutex<Option<(String, abner_processor_api::Progress)>>>,
    frame: Arc<Mutex<FrameState>>,
    done: mpsc::Receiver<Result<PathBuf, String>>,
    cancel: Cancellation,
    active: Arc<Mutex<Vec<Cancellation>>>,
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
    history: Vec<Page>,
    record: Option<PathBuf>,
    preview_source: Option<PathBuf>,
    queue_scroll: usize,
    bitrate_mbps: u32,
    bitrate_edit: Option<String>,
    export_lossless: bool,
    review: Option<crate::player::Player>,
    review_pending: bool,
    review_fps: f64,
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
            history: Vec::new(),
            record: None,
            preview_source: None,
            queue_scroll: 0,
            bitrate_mbps: 40,
            bitrate_edit: None,
            export_lossless: false,
            review: None,
            review_pending: false,
            review_fps: 30.,
        }
    }
}
impl Drop for Page {
    fn drop(&mut self) {
        self.cancel();
    }
}
const CANVAS_RADIUS: f32 = 10.;
#[derive(Clone, Copy)]
pub(crate) struct Layout {
    pub sidebar: RectPx,
    pub video: RectPx,
    pub canvas: Canvas,
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
        let attachment_h = (vp.1 * 0.325).clamp(176., 260.);
        // Figma 621:8921: no bar; the picture meets the header and the
        // attachment runs transport, review track, then the job card.
        let canvas = Canvas::layout(RectPx{x:side,y:sidebar.y,w:vp.0-side,h:footer_y-sidebar.y},false,
            Some(TimelineAttachment{x:side,width:vp.0-side,transport_height:attachment_h,lanes_height:0.}));
        let transport=canvas.timeline.unwrap().transport;
        let video=canvas.r;
        let seek = RectPx { x: transport.x + 38., y: transport.y + 66.,
            w: (transport.w - 76.).max(80.), h: 4. };

        Self {
            sidebar,
            video,
            canvas,
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
/// Export owns its content slots, while Canvas and ButtonBar retain shared shell geometry.
struct SettingsLayout { width:f32, codec:f32, container:f32, filename:RectPx, directory:RectPx, export:RectPx }
impl SettingsLayout {
    fn new(l:Layout)->Self {
        let width=l.sidebar.w-32.;
        let scale=((l.sidebar.h-50.)/668.).clamp(0.72,1.);
        let y=|offset:f32| l.sidebar.y+offset*scale;
        Self {width,codec:y(130.),container:y(182.),
            filename:RectPx{x:16.,y:y(266.),w:width,h:32.},
            directory:RectPx{x:16.,y:y(336.),w:width,h:32.},
            export:RectPx{x:16.,y:y(406.),w:width,h:31.}}
    }
}
impl Layout {
    /// Under the transport row and the review track, 16px from the attachment's edges.
    fn job_card(&self)->RectPx { RectPx{x:self.transport.x+16.,y:self.transport.y+84.,w:self.transport.w-32.,h:(self.transport.h-100.).max(76.)} }
}
fn surface(items:&mut Vec<Item>,r:RectPx,color:u32,radius:f32) { surface_alpha(items,r,hex_color(color),radius); }
fn surface_alpha(items:&mut Vec<Item>,r:RectPx,color:[f32;4],radius:f32) {
    items.push(Item::Shape(figleaf::Shape::rect(figleaf::Rect::new(r.x,r.y,r.w.max(0.),r.h.max(0.))).radius(radius).fill(color)));
}
fn export_label(items:&mut Vec<Item>,r:RectPx,value:&str,size:f32,color:[f32;4],mono:bool,align:Align) {
    use figleaf::{Text,Rect,paint::Align as Place};
    let bounds=Rect::new(r.x,r.y,r.w.max(1.),r.h.max(1.));
    items.push(Item::Label(Text::new(value,bounds).font(if mono{3}else{2}).size(size,size*1.4).color(color)
        .align(match align {Align::Left=>Place::Start,Align::Center=>Place::Center,Align::Right=>Place::End},Place::Center).clip(bounds)));
}
fn duration_label(time:f64)->String { let s=time.max(0.) as u64;format!("{:02}:{:02}",s/60,s%60) }
fn timecode(time:f64,fps:f64)->String {
    let s=time.max(0.) as u64;format!("{:02}:{:02}:{:02}:{:02}",s/3600,s/60%60,s%60,(time.fract()*fps.max(1.)).floor() as u64)
}

fn hit(r: RectPx, x: f32, y: f32) -> bool {
    x >= r.x && y >= r.y && x < r.x + r.w && y < r.y + r.h
}
impl Page {
    /// The total bar while a job runs: the share of the clip written, plus the
    /// working frame's share (its stage's steps through the current call,
    /// placed within the chain) of one frame's length. Never decreases.
    fn total_fraction(&self) -> f64 {
        use abner_processing::progress::{FRAME_CAP, frame_share};
        let duration = self.duration.max(0.001);
        let done = (self.rendered_time / duration).clamp(0., 1.);
        let Some(job) = &self.job else { return done };
        let Ok(mut state) = job.frame.lock() else { return done };
        let frame = self.frames.back().map_or(1. / self.review_fps.max(1.), |f| f.frame_duration);
        let share = state.report.as_ref()
            .and_then(|(_, i, n, f)| f.fraction().map(|x| frame_share(*i, *n, x, f.frame_count())))
            .unwrap_or(0.);
        // A batch call's share is several frames long; it never finishes the clip.
        let total = if done >= 1. { 1. } else { (done + share * frame / duration).min(1. - (1. - FRAME_CAP) * frame / duration) };
        state.floor = state.floor.max(total);
        state.floor
    }
    fn frame_report(&self) -> Option<FrameReport> {
        self.job.as_ref().and_then(|j| j.frame.lock().ok()?.report.clone())
    }
    fn swap_preview(&mut self, other: &mut Self) {
        std::mem::swap(&mut self.export_lossless, &mut other.export_lossless);
        std::mem::swap(&mut self.review, &mut other.review);
        std::mem::swap(&mut self.review_pending, &mut other.review_pending);
        std::mem::swap(&mut self.review_fps, &mut other.review_fps);
        std::mem::swap(&mut self.job, &mut other.job);
        std::mem::swap(&mut self.frames, &mut other.frames);
        std::mem::swap(&mut self.selected, &mut other.selected);
        std::mem::swap(&mut self.playback, &mut other.playback);
        std::mem::swap(&mut self.held, &mut other.held);
        std::mem::swap(&mut self.started, &mut other.started);
        std::mem::swap(&mut self.elapsed, &mut other.elapsed);
        std::mem::swap(&mut self.status, &mut other.status);
        std::mem::swap(&mut self.path, &mut other.path);
        std::mem::swap(&mut self.duration, &mut other.duration);
        std::mem::swap(&mut self.count, &mut other.count);
        std::mem::swap(&mut self.rendered_time, &mut other.rendered_time);
        std::mem::swap(&mut self.seek, &mut other.seek);
        std::mem::swap(&mut self.dirty, &mut other.dirty);
        std::mem::swap(&mut self.following, &mut other.following);
        std::mem::swap(&mut self.original, &mut other.original);
        std::mem::swap(&mut self.record, &mut other.record);
        std::mem::swap(&mut self.preview_source, &mut other.preview_source);
    }
    fn archive(&mut self) {
        if self.record.is_none() && self.job.is_none() { return; }
        let mut page = Self::default();
        self.swap_preview(&mut page);
        page.source = page.preview_source.clone();
        self.history.push(page);
        self.queue_scroll = 0;
    }
    pub fn has_jobs(&self) -> bool { self.busy() || self.history.iter().any(Self::busy) }
    fn queue_rect(&self, vp: (f32,f32)) -> RectPx {
        let l = Layout::new(vp);
        let y = self.settings(l).export.y + 45.;
        RectPx { x: 0., y, w: l.sidebar.w-1., h: (l.footer.y-y).max(0.) }
    }
    pub fn scroll(&mut self, dy: f32, cursor: (f32,f32), vp:(f32,f32)) -> bool {
        if !hit(self.queue_rect(vp),cursor.0,cursor.1) { return false; }
        let total = self.history.len() + usize::from(self.record.is_some());
        if dy < 0. { self.queue_scroll = (self.queue_scroll+1).min(total.saturating_sub(1)); }
        else if dy > 0. { self.queue_scroll = self.queue_scroll.saturating_sub(1); }
        true
    }
    fn settings(&self, l: Layout) -> SettingsLayout {
        SettingsLayout::new(l)
    }
    pub fn editing_filename(&self) -> bool { self.open && (self.filename_edit.is_some() || self.bitrate_edit.is_some()) }
    pub fn finish_filename(&mut self) -> bool {
        if let Some(value) = &self.bitrate_edit {
            match value.parse::<u32>() {
                Ok(value @ 1..=1000) => { self.bitrate_mbps = value; self.bitrate_edit = None; }
                _ => { self.status = "Enter a target bitrate from 1 to 1000 Mbps.".into(); return false; }
            }
        }
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
        if let Some(value) = self.bitrate_edit.as_mut() {
            let filtered: String = text.chars().filter(char::is_ascii_digit).collect();
            self.filename_cursor.insert(value, &filtered, 4);
            return true;
        }
        let value = self.filename_edit.as_mut().unwrap();
        let filtered: String = text.chars().filter(|c| !c.is_control() && !matches!(c, '/' | ':')).collect();
        self.filename_cursor.insert(value, &filtered, usize::MAX);
        true
    }
    /// A press on the filename field: the caret goes under the pointer and
    /// a drag from here selects.
    pub(crate) fn press_text(&mut self, x: f32) {
        if self.filename_edit.is_some() { self.filename_cursor.press(x); }
    }
    pub(crate) fn drag_text(&mut self, x: f32) -> bool {
        self.editing_filename() && self.filename_cursor.drag(x)
    }
    pub(crate) fn release_text(&mut self) { self.filename_cursor.release(); }
    pub(crate) fn text_layout(&mut self, layout: crate::text_edit::FieldLayout) -> bool {
        self.filename_cursor.layout(layout) && self.editing_filename()
    }
    pub(crate) fn next_blink(&self) -> std::time::Instant { self.filename_cursor.next_blink() }
    pub(crate) fn edit_text(&mut self, edit: Edit) -> bool {
        if !self.editing_filename() { return false; }
        if matches!(edit, Edit::Paste) {
            if let Some(text) = crate::open::paste_text() { self.text_input(&text); }
        } else {
            self.filename_cursor.apply(self.bitrate_edit.as_mut().or(self.filename_edit.as_mut()).unwrap(), edit);
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
    /// A destination chosen up front (`--export-run OUT`): the container
    /// follows its extension and the Export press skips the save panel. The
    /// file must not exist yet — an export never overwrites.
    pub fn set_destination(&mut self, path: PathBuf) -> Result<(), String> {
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
        self.container = match ext.as_str() {
            "mp4" => Container::Mp4,
            "mov" => Container::Mov,
            "mkv" => Container::Mkv,
            _ => return Err(format!("{}: export to .mp4, .mov or .mkv", path.display())),
        };
        if path.exists() {
            return Err(format!("{} already exists; exports never overwrite", path.display()));
        }
        self.destination = Some(path);
        Ok(())
    }
    pub fn set_copy_unavailable(&mut self, reason: Option<&'static str>) {
        self.copy_unavailable = reason;
        if reason.is_some() { self.lossless = false; }
    }
    pub fn busy(&self) -> bool {
        self.job.is_some()
    }
    pub fn cancel(&mut self) {
        if self.job.as_ref().is_some_and(|j|j.launch.is_some()) {
            self.job = None;
            self.status = "Export cancelled".into();
            return;
        }
        if let Some(j) = &self.job {
            j.cancel.cancel();
            j.copy_cancel.store(true, std::sync::atomic::Ordering::Relaxed);
            for c in j.active.lock().unwrap().iter() {
                c.cancel();
            }
            self.status = "Cancelling export…".into();
        }
    }
    pub fn new_source(&mut self) {
        self.archive();
        self.source = None;
    }
    pub fn reset_source(&mut self, info: &VideoInfo) {
        if self.source.as_ref() != Some(&info.path) {
            self.archive();
            self.crop = None;
            self.destination = None;
            self.filename_edit = None;
            self.source = Some(info.path.clone());
            self.duration = info.duration;
            self.seek = None;
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
        if self.history.iter().any(|p| p.record.as_ref() == Some(&dest) && p.busy())
            || (self.busy() && self.record.as_ref() == Some(&dest)) {
            self.status = "An export is already writing this filename.".into();
            return;
        }
        self.archive();
        self.record = Some(dest.clone());
        self.export_lossless = self.lossless;
        self.preview_source = Some(info.path.clone());
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
        // The source's rate until the output's first frame says otherwise: the
        // total bar sizes an in-flight call's frames by it, and 30 fps read a
        // 24 fps clip's first batch a fifth short.
        if info.fps > 0. { self.review_fps = info.fps; }
        self.started = Instant::now();
        self.elapsed = 0.;
        self.status = if self.lossless { "Copying compressed media…" } else { "Preparing pipeline…" }.into();
        let cancel = Cancellation::default();
        let latest = Arc::new(Mutex::new(None));
        let (tx, done) = mpsc::sync_channel(1);
        let shared = latest.clone();
        let stage = Arc::new(Mutex::new(None));
        let frame = Arc::new(Mutex::new(FrameState::default()));
        let report: StageReport = {
            let (stage, frame) = (stage.clone(), frame.clone());
            Arc::new(move |name, update| match update {
                StageUpdate::Phase(p) => *stage.lock().unwrap() = Some((name.to_owned(), p)),
                StageUpdate::Frame(index, count, Some(f)) =>
                    frame.lock().unwrap().report = Some((name.to_owned(), index, count, f)),
                StageUpdate::Frame(index, _, None) => {
                    let mut frame = frame.lock().unwrap();
                    if frame.report.as_ref().is_some_and(|f| f.1 == index) { frame.report = None; }
                }
            })
        };
        let stop = cancel.clone();
        let inputs = self.inputs.clone();
        let chapters = self.chapters.clone();
        let prores = self.prores;
        let bitrate_kbps = self.bitrate_mbps * 1000;
        let lossless = self.lossless;
        let crop = self.crop;
        let active = Arc::new(Mutex::new(Vec::new()));
        let worker_active = active.clone();
        let copy_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let worker_copy_cancel = copy_cancel.clone();
        let launch: Box<dyn FnOnce() + Send> = Box::new(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                stop.check().map_err(|e| e.to_string())?;
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
                crate::media_io::pipeline_export_with_inputs(
                    &inputs,
                    &info,
                    &dest,
                    &keeps,
                    &chapters,
                    &*registry,
                    &chain,
                    prores,
                    Some(bitrate_kbps),
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
            launch: Some(launch),
            latest,
            stage,
            frame,
            done,
            cancel,
            active,
            copy_cancel,
        });
    }
    pub fn poll(&mut self, uploads: &mut Vec<Upload>, slot: usize) {
        let running = std::iter::once(&*self).chain(self.history.iter())
            .any(|p|p.job.as_ref().is_some_and(|j|j.launch.is_none()));
        if !running {
            let next = std::iter::once(&*self).chain(self.history.iter()).enumerate()
                .filter(|(_,p)|p.job.is_some()).min_by_key(|(_,p)|p.started).map(|(i,_)|i);
            if let Some(index) = next {
                let page = if index == 0 { &mut *self } else { &mut self.history[index-1] };
                page.started = Instant::now();
                if let Some(launch) = page.job.as_mut().and_then(|j|j.launch.take()) { std::thread::spawn(launch); }
            }
        }
        for page in &mut self.history { page.poll_current(&mut Vec::new(), slot); }
        self.poll_current(uploads, slot);
        if self.review.is_some() || self.preview_source != self.source { self.seek = None; self.original = false; }
    }
    fn open_output(&mut self) {
        let Some(path) = &self.path else { return; };
        match crate::probe::probe(path) {
            Ok(info) => {
                self.review = crate::player::Player::spawn(path, info.width, info.height,
                    crate::probe::vt_accel(&info.codec), info.rotation, 128 * 1024 * 1024);
                self.review_pending = self.review.is_some();
                self.review_fps = info.fps.max(1.);
                self.duration = info.duration;
                self.frames.clear(); self.held = None; self.selected = 0;
                self.following = true; self.original = false; self.playback = None;
            }
            Err(e) => self.status = format!("Cannot open export: {e}"),
        }
    }
    fn review_seek(&mut self, time: f64) {
        if let Some(player) = &self.review {
            player.seek(time.clamp(0., (self.duration - 1./self.review_fps).max(0.)), true);
            self.review_pending = true;
            self.playback = None;
        }
    }
    fn poll_current(&mut self, uploads: &mut Vec<Upload>, slot: usize) {
        if let Some(job) = &self.job
            && let Some(frame) = job
                .latest
                .lock()
                .unwrap()
                .take()

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
        let result = self.job.as_ref().and_then(|j| match j.done.try_recv() {
            Ok(r) => Some(r),
            Err(mpsc::TryRecvError::Disconnected) => Some(Err("Export worker stopped".into())),
            Err(mpsc::TryRecvError::Empty) => None,
        });
        if let Some(result) = result {
            self.elapsed = self.started.elapsed().as_secs_f64();
            self.job = None;
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
        if let Some(player) = &self.review {
            let time = self.playback.map(|(start, pos)| pos + start.elapsed().as_secs_f64());
            let frame = if self.review_pending { player.take_next() }
                else { time.and_then(|t|player.take_upto(t)) };
            if let Some((pts, pixels)) = frame {
                self.review_pending = false;
                self.frames.clear(); self.held = None; self.selected = 0; self.following = true;
                self.frames.push_back(Rendered { pixels, dims:[player.w,player.h], source_pts:pts,
                    output_time:pts+1./self.review_fps, count:(pts*self.review_fps).round() as u64,
                    frame_duration:1./self.review_fps });
                self.dirty = true;
            }
            if time.is_some_and(|t| t >= self.duration) { self.playback = None; }
        }
        if self.review.is_none() && let Some((started, position)) = self.playback {
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
        if self.review.is_some() {
            let time = self.current().map_or(0., Self::frame_time);
            self.review_seek(time + if forward { 0.5 / self.review_fps } else { -1.5 / self.review_fps });
            return;
        }
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
                Key::Escape => { self.bitrate_edit = None; self.filename_edit = None; self.filename_cursor = Cursor::default(); }
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
        let settings = self.settings(l);
        let third = (settings.width - 4.) / 3.;
        let r = |col: f32, y| RectPx { x: 16. + col*(third+2.), y, w: third, h: 21. };
        let mut actions = vec![
            (r(0.,settings.codec), "Copy", self.lossless, Action::Lossless),
            (r(1.,settings.codec), "HEVC", !self.prores && !self.lossless, Action::Preset(false)),
            (r(2.,settings.codec), "ProRes", self.prores && !self.lossless, Action::Preset(true)),
            (r(0.,settings.container), "MOV", self.container == Container::Mov, Action::Container(Container::Mov)),
            (r(1.,settings.container), "MP4", self.container == Container::Mp4, Action::Container(Container::Mp4)),
            (r(2.,settings.container), "MKV", self.container == Container::Mkv, Action::Container(Container::Mkv)),
            (settings.filename, "", self.filename_edit.is_some(), Action::EditFilename),
            (RectPx{x: settings.directory.x+settings.directory.w-38.,w:38.,..settings.directory}, "Change directory", false, Action::ChooseDirectory),
            (RectPx{w:settings.directory.w-38.,..settings.directory}, "", false, Action::RevealDirectory),
            (settings.export, "Export", false, Action::Export),
        ];
        let cy = l.seek.y - 50.;
        // The time/follow row collapses gracefully while retaining all five transport controls.
        let center = if l.transport.w < 700. { l.transport.x+110. } else { l.transport.x+l.transport.w/2. };
        for (offset, width, label, action) in [(-93.,34.,"Previous keyframe",Action::Keyframe(false)),
            (-55.,34.,"Previous frame",Action::Step(false)),(-17.,34.,"Play / pause",Action::Play),
            (21.,34.,"Next frame",Action::Step(true)),(59.,34.,"Next keyframe",Action::Keyframe(true))] {
            actions.push((RectPx{x:center+offset,y:cy,w:width,h:34.},label,false,action));
        }
        actions.push((RectPx{x:l.transport.x+l.transport.w-192.,y:cy,w:154.,h:34.},"Follow latest frame",self.following,Action::Follow));
        let card = l.job_card();
        if self.busy() { actions.push((RectPx{x:card.x+card.w-146.,y:card.y+if card.h<110.{4.}else{20.},w:130.,h:if card.h<110.{30.}else{38.}},"Cancel",false,Action::Cancel)); }
        else if self.path.is_some() { actions.push((RectPx{x:card.x+card.w-146.,y:card.y+if card.h<110.{4.}else{20.},w:130.,h:if card.h<110.{30.}else{38.}},"Reveal output",false,Action::Reveal)); }
        let queue = self.queue_rect(vp);
        for (row, index) in (0..self.history.len() + usize::from(self.record.is_some())).rev().skip(self.queue_scroll).enumerate() {
            let rect = RectPx { x: 8., y: queue.y + 24. + row as f32 * 38., w: l.sidebar.w - 16., h: 34. };
            if rect.y + rect.h > l.footer.y { break; }
            actions.push((rect, "", index == self.history.len(), Action::OpenExport(index)));
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
        if !matches!(a, Action::EditFilename) && !self.finish_filename() { return None; }
        match a {
            Action::OpenExport(index) => {
                if index < self.history.len() {
                    let mut page = self.history.remove(index);
                    self.swap_preview(&mut page);
                    if page.record.is_some() || page.busy() { self.history.insert(index, page); }
                }
                self.original = false;
                self.dirty = true;
                self.open_output();
            }
            Action::EditFilename => {
                if self.filename_edit.is_none() {
                    self.filename_edit = Some(self.destination_name());
                    self.filename_cursor = Cursor::default();
                }
            }
            Action::Preset(p) => {
                self.lossless = false;
                self.prores = p;
                if p && self.container == Container::Mp4 {
                    self.container = Container::Mov;
                }
            }
            Action::Lossless if self.copy_unavailable.is_none() => {
                self.lossless = true;
                self.prores = false;
            }
            Action::Container(container) => {
                if container == Container::Mp4 && self.prores && !self.lossless {
                    self.status = "ProRes requires MOV or MKV.".into();
                } else {
                    self.container = container;
                }
            }
            Action::ChooseDirectory => {
                if let Some(dir) = crate::open::export_directory(self.directory().as_deref()) {
                    self.destination = Some(dir.join(self.destination_name()));
                }
            }
            Action::Keyframe(forward) => {
                if let Some(player) = &self.review {
                    let time = self.current().map_or(0., Self::frame_time);
                    player.seek_biased((time + if forward { 0.5/self.review_fps } else { -0.5/self.review_fps }).max(0.),false,forward);
                    self.review_pending = true; self.playback = None;
                } else if !self.frames.is_empty() {
                    // Live previews are unencoded frames; their available boundaries are the navigation anchors.
                    self.following=false; self.playback=None;
                    self.show_frame(if forward { self.frames.len()-1 } else { 0 });
                }
            }
            Action::Cancel => self.cancel(),
            Action::Play => self.toggle_playback(),
            Action::Step(forward) => self.step_preview(forward),
            Action::Follow => {
                self.playback = None;
                self.following = !self.following;
                self.show_frame(self.frames.len().saturating_sub(1));
            }
            Action::Seek(fraction) if self.review.is_some() => self.review_seek(fraction * self.duration),
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
    // Keep the explicit geometry/pipeline inputs at this boundary.
    #[allow(clippy::too_many_arguments)]
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
        use crate::render::{TextItem, VAlign};
        let l = Layout::new(vp);
        let settings = self.settings(l);
        let ink = hex_color(0xf4f3ed);
        let dim = hex_color(0xc6c8c0);
        surface(items,l.sidebar,0x050505,0.);
        surface(items,l.transport,0x000000,0.);
        let corners=[CANVAS_RADIUS,CANVAS_RADIUS,0.,0.];
        l.canvas.ground_radii(items,corners);
        let current = self.current();
        let idx = if current.is_some() && !self.original { slot } else { source };
        let picture = crate::canvas::zoomed(self.base_rect(vp,show_ui,info),view.0,view.1);
        l.canvas.clip(items);
        Canvas::lift(items,picture);
        if self.original && current.is_some_and(|f| (f.source_pts-shown_pts).abs()>0.51/info.fps.max(1.)) {
            export_label(items,l.video,"Seeking matching original…",12.,dim,false,Align::Center);
        } else { items.push(Item::Video{a:idx,b:idx,r:picture,uv:[0.,0.,1.,1.],mode:VideoMode::Tex,p0:0.,p1:0.}); }
        l.canvas.recess_radii(items,corners);
        items.push(Item::Clip(None));
        export_label(items,RectPx{x:16.,y:l.sidebar.y+8.,w:settings.width,h:22.},"Export Settings",13.,hex_color(0xc8c8c8),true,Align::Left);
        let thumb=RectPx{x:16.,y:l.sidebar.y+38.,w:72.,h:40.5};
        items.push(Item::Video{a:source,b:source,r:crate::canvas::fit_rect((info.width,info.height),thumb),uv:[0.,0.,1.,1.],mode:VideoMode::Tex,p0:0.,p1:0.});
        export_label(items,RectPx{x:98.,y:thumb.y,w:settings.width-82.,h:18.},&info.path.file_name().unwrap_or_default().to_string_lossy(),11.,ink,true,Align::Left);
        export_label(items,RectPx{x:98.,y:thumb.y+22.,w:settings.width-82.,h:14.},&format!("{} · {}p · {:.0}fps",info.codec,info.height,info.fps),8.,hex_color(0x8b8b8b),true,Align::Left);
        for y in [settings.codec-34.,settings.filename.y-42.,settings.export.y-20.].into_iter().filter(|_|l.sidebar.h>=550.) {
            surface(items,RectPx{x:16.,y,w:settings.width,h:1.},0x303030,0.);
        }
        for (title,y) in [("Codec",settings.codec-15.),("Container",settings.container-15.),("Filename",settings.filename.y-18.),("Directory",settings.directory.y-18.)] {
            export_label(items,RectPx{x:16.,y,w:settings.width,h:12.},title,8.,hex_color(0xc8c8c8),true,Align::Left);
        }
        // Figma 547:5962 / 547:5966: light recessed fields with dark type.
        // The edge is the frame's white @ 62% over the fill, baked opaque.
        let field_ink = hex_color(0x222422);
        let field = |items:&mut Vec<Item>,r:RectPx| {
            items.push(Item::Shape(figleaf::Shape::rect(figleaf::Rect::new(r.x,r.y,r.w,r.h)).radius(7.)
                .fill(hex_color(0x9fa89f)).stroke(1.,hex_color(0xdbdedb),figleaf::StrokeAlign::Inside)
                .shadow(figleaf::Shadow::inner([0.,0.,0.,0.45],[0.,2.],6.))));
        };
        field(items,settings.filename);
        field(items,settings.directory);
        export_label(items,RectPx{x:settings.directory.x+12.,w:settings.directory.w-54.,..settings.directory},
            &self.directory().map(|p|p.display().to_string()).unwrap_or_default(),11.,field_ink,true,Align::Left);
        let queue=self.queue_rect(vp);
        if self.record.is_some() || !self.history.is_empty() {
            surface(items,queue,0x0f0f0f,0.);
            export_label(items,RectPx{x:16.,y:queue.y,w:settings.width,h:24.},&format!("Exports · {}",self.history.len()+usize::from(self.record.is_some())),11.,ink,true,Align::Left);
        }
        let card=l.job_card();
        items.push(Item::Shape(figleaf::Shape::rect(figleaf::Rect::new(card.x,card.y,card.w,card.h)).radius(8.)
            .fill(hex_color(0x0f0e0e)).stroke(1.,hex_color(0x2b2a2a),figleaf::StrokeAlign::Inside)));
        let done=if self.busy() {self.total_fraction()} else {(self.rendered_time/self.duration.max(0.001)).clamp(0.,1.)};
        let queued=self.job.as_ref().is_some_and(|j|j.launch.is_some());
        let title=if queued { "Queued".into() } else if self.busy() {
            if self.export_lossless { "Copying packets…".into() } else if self.count==0 && done<=0. { "Preparing export…".into() }
            else { format!("Rendering & encoding   {:.0}%",done*100.) }
        } else if self.path.is_some() { "Export complete".into() } else if self.record.is_some() { self.status.clone() } else { "Ready to export".into() };
        let elapsed=if self.busy() {self.started.elapsed().as_secs_f64()}else{self.elapsed};
        let subtitle=if self.busy() && !queued && done>0. && done<1. {
            format!("About {} remaining",remaining(elapsed*(1.-done)/done))
        } else if queued { "Waiting for the previous export".into() }
        else if self.busy() { "Waiting for progress…".into() }
        else { self.path.as_ref().map(|p|p.display().to_string()).unwrap_or_else(||"Choose a preset and export the enabled pipeline.".into()) };
        let text_w=card.w-if self.busy()||self.path.is_some(){184.}else{40.};
        export_label(items,RectPx{x:card.x+20.,y:card.y+if card.h<110.{4.}else{14.},w:text_w,h:26.},&title,if card.h<110.{14.}else{18.},ink,false,Align::Left);
        if card.h>=110. {
            export_label(items,RectPx{x:card.x+20.,y:card.y+42.,w:text_w,h:20.},&subtitle,13.,dim,false,Align::Left);
        }
        let progress=RectPx{x:card.x+20.,y:card.y+card.h-if card.h<110.{32.}else{42.},w:card.w-40.,h:6.};
        surface(items,progress,0x202020,0.);
        surface(items,RectPx{w:progress.w*if self.path.is_some(){1.}else{done as f32},..progress},0x0059ff,0.);
        // The frame bar: the working stage's steps through the frame in hand,
        // for processors slow enough to report them.
        if card.h>=110. && self.busy() && let Some((_,_,_,f))=self.frame_report() && let Some(step)=f.fraction() {
            let lane=RectPx{y:progress.y+progress.h+3.,h:3.,..progress};
            surface(items,lane,0x181818,0.);
            surface(items,RectPx{w:lane.w*step as f32,..lane},0x2d5fa8,0.);
        }
        let stats_y=card.y+card.h-24.;
        let third=(card.w-40.)/3.;
        for (i,text,align) in [(0,format!("{} of {} processed",duration_label(self.rendered_time),duration_label(self.duration)),Align::Left),
            (1,format!("{:.1} fps",self.count as f64/elapsed.max(0.001)),Align::Center),
            (2,format!("{} elapsed",remaining(elapsed)),Align::Right)] {
            export_label(items,RectPx{x:card.x+20.+i as f32*third,y:stats_y,w:third,h:20.},&text,if card.w<600.{9.}else{12.},dim,false,align);
        }
        let time=current.map_or(0.,Self::frame_time);
        if l.transport.w>=700. {
            export_label(items,RectPx{x:l.seek.x,y:l.seek.y-50.,w:185.,h:34.},&timecode(time,current.map_or(info.fps,|f|1./f.frame_duration)),16.,ink,true,Align::Left);
        }
        surface(items,l.seek,0x000000,0.);
        surface(items,RectPx{w:l.seek.w*(time/self.duration.max(0.001)).clamp(0.,1.) as f32,..l.seek},0x0059ff,0.);
        surface(items,l.footer,0x050505,0.);
        export_label(items,RectPx{x:16.,y:l.footer.y,w:70.,h:l.footer.h},"EXPORT",10.,hex_color(0x8b8b8b),false,Align::Left);
        let status=if self.busy() {render_line(done,self.count,self.frames.back().map(|f|f.frame_duration),self.duration,elapsed,self.job.as_ref().and_then(|j|j.stage.lock().ok().and_then(|s|s.clone())).as_ref(),self.frame_report().as_ref())}else{self.status.clone()};
        export_label(items,RectPx{x:98.,y:l.footer.y,w:vp.0-114.,h:l.footer.h},&status,10.,hex_color(0x8b8b8b),false,Align::Left);
        for (r,t,selected,a) in self.actions(vp) {
            let hovered=hit(r,cursor.0,cursor.1);
            let disabled=match a {
                Action::Play|Action::Step(_)|Action::Keyframe(_)=>self.frames.is_empty(),
                Action::Lossless=>self.copy_unavailable.is_some(),
                Action::Container(Container::Mp4)=>self.prores&&!self.lossless,
                _=>false,
            };
            match a {
                Action::ChooseDirectory=>{
                    items.push(Item::Shape(figleaf::Shape::rect(figleaf::Rect::new(r.x,r.y,r.w,r.h)).radii([0.,7.,7.,0.]).fill(hex_color(if hovered{0x484948}else{0x343534}))));
                    items.push(Item::FrameIcon{x:r.x+(r.w-20.)/2.,y:r.y+(r.h-20.)/2.,index:6});
                }
                Action::RevealDirectory=>{},
                Action::Export|Action::Cancel|Action::Reveal=>{
                    ButtonBar::primary(items,r,t,hovered,false);
                }
                Action::Follow=>{
                    export_label(items,RectPx{w:r.w-42.,..r},t,12.,ink,false,Align::Right);
                    let switch=RectPx{x:r.x+r.w-32.,y:r.y+9.,w:32.,h:16.};
                    items.push(Item::Shape(figleaf::Shape::rect(figleaf::Rect::new(switch.x,switch.y,switch.w,switch.h)).radius(8.).fill(hex_color(if selected{0xff9d00}else{0x414141})).stroke(1.,ink,figleaf::StrokeAlign::Inside)));
                    surface(items,RectPx{x:switch.x+if selected{18.}else{2.},y:switch.y+2.,w:12.,h:12.},0xf4f3ed,6.);
                }
                Action::Play|Action::Step(_)|Action::Keyframe(_)=>{
                    let index=match a {Action::Keyframe(false)=>7,Action::Step(false)=>8,Action::Play=>9,Action::Step(true)=>10,_=>11};
                    if hovered&&!disabled {surface(items,r,0x303030,6.);}
                    items.push(Item::FrameIcon{x:r.x,y:r.y+if a==Action::Play{0.}else{5.},index});
                    if self.playing() && a==Action::Play {
                        surface(items,r,0x168be0,17.);
                        for x in [r.x+12.,r.x+20.] {surface(items,RectPx{x,y:r.y+10.,w:3.,h:14.},0xffffff,0.);}
                    }
                    // A veil of the attachment's own black dims a dead control.
                    if disabled {surface_alpha(items,r,[0.,0.,0.,0.65],if a==Action::Play{17.}else{0.});}
                }
                Action::EditFilename=>{
                    let value=self.filename_edit.clone().unwrap_or_else(||self.destination_name());
                    items.push(Item::Text(TextItem{regular:true,valign:VAlign::Middle,max_width:Some(r.w-24.),
                        ..TextItem::new(r.x+12.,r.y+r.h/2.,11.,field_ink,value)}));
                    if self.filename_edit.is_some() && let Some(Item::Text(text)) = items.last_mut() { self.filename_cursor.show(text); }
                }
                Action::OpenExport(index)=>{
                    surface(items,r,if selected{0x19334b}else if hovered{0x303438}else{0x171c21},6.);
                    let page=self.history.get(index).unwrap_or(self);
                    let name=page.record.as_ref().and_then(|p|p.file_name()).unwrap_or_default().to_string_lossy();
                    export_label(items,RectPx{x:r.x+8.,y:r.y+2.,w:r.w-16.,h:15.},&name,10.,ink,true,Align::Left);
                    let state=if page.busy(){if page.job.as_ref().is_some_and(|j|j.launch.is_some()){"Queued".into()}else{format!("Rendering · {:.0}%",page.total_fraction()*100.)}}else if page.path.is_some(){"Completed · click to view".into()}else{page.status.clone()};
                    export_label(items,RectPx{x:r.x+8.,y:r.y+17.,w:r.w-16.,h:15.},&state,9.,dim,true,Align::Left);
                }
                _=>{
                    let border=hex_color(if selected{0xff8d28}else if disabled{0x393939}else{0x666666});
                    items.push(Item::Shape(figleaf::Shape::rect(figleaf::Rect::new(r.x,r.y,r.w,r.h)).radius(5.).fill(hex_color(if hovered&&!disabled{0x242424}else{0x050505})).stroke(if selected{2.}else{1.},border,figleaf::StrokeAlign::Inside)));
                    export_label(items,r,t,10.,if disabled{hex_color(0x686868)}else{ink},true,Align::Center);
                }
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
            for p in f.pixels.as_chunks_mut::<3>().0.iter_mut() {
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
    fn set_destination_picks_the_container_and_refuses_overwrites() {
        let dir = std::env::temp_dir().join(format!("abner-dest-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut p = Page::default();
        p.set_destination(dir.join("out.MOV")).unwrap();
        assert!(p.container == Container::Mov);
        assert_eq!(p.destination.as_deref(), Some(dir.join("out.MOV").as_path()));
        assert!(p.set_destination(dir.join("out.avi")).is_err());
        std::fs::write(dir.join("taken.mkv"), b"").unwrap();
        assert!(p.set_destination(dir.join("taken.mkv")).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
    #[test]
    fn bitrate_input_defaults_validates_and_preserves_last_valid_value() {
        let mut p = Page::default(); p.open = true;
        assert_eq!(p.bitrate_mbps, 40);
        p.bitrate_edit = Some(p.bitrate_mbps.to_string()); p.filename_cursor.apply(p.bitrate_edit.as_mut().unwrap(), Edit::SelectAll); p.text_input("65");
        assert!(p.finish_filename()); assert_eq!(p.bitrate_mbps, 65);
        p.bitrate_edit = Some(p.bitrate_mbps.to_string()); p.filename_cursor.apply(p.bitrate_edit.as_mut().unwrap(), Edit::SelectAll); p.text_input("0");
        assert!(!p.finish_filename()); assert_eq!(p.bitrate_mbps, 65);
        p.key(Key::Escape); assert!(!p.editing_filename());
    }

    #[test]
    fn archived_export_keeps_worker_completion_and_selectable_preview() {
        let mut p = Page::default(); p.open = true;
        p.source = Some(PathBuf::from("old.mp4")); p.preview_source = p.source.clone();
        p.record = Some(PathBuf::from("output.mp4"));
        let cancel = Cancellation::default();
        let (tx, done) = mpsc::channel();
        p.job = Some(Job { launch: None, latest: Arc::default(), stage: Arc::default(), frame: Arc::default(), done,
            cancel: cancel.clone(), active: Arc::default(), copy_cancel: Arc::default() });
        p.archive(); p.source = Some(PathBuf::from("new.mp4"));
        assert!(cancel.check().is_ok()); assert!(p.has_jobs());
        assert_eq!(p.history.len(), 1);
        tx.send(Ok(PathBuf::from("output.mp4"))).unwrap();
        p.poll(&mut vec![], 3);
        assert!(!p.has_jobs());
        assert_eq!(p.history[0].path.as_deref(), Some(std::path::Path::new("output.mp4")));
        assert!(p.actions((720.,480.)).iter().any(|(_,_,_,a)| *a == Action::OpenExport(0)));
        assert_eq!(p.source.as_deref(), Some(std::path::Path::new("new.mp4")));
        p.apply(Action::OpenExport(0));
        assert!(p.history.is_empty(), "selecting history must not create an empty queue row");
        assert_eq!(p.record.as_deref(), Some(std::path::Path::new("output.mp4")));
    }

    #[test]
    fn total_bar_folds_the_working_frame_and_never_moves_back() {
        let mut p = Page::default();
        let (_tx, done) = mpsc::channel();
        let frame: Arc<Mutex<FrameState>> = Arc::default();
        p.job = Some(Job { launch: None, latest: Arc::default(), stage: Arc::default(), frame: frame.clone(), done,
            cancel: Cancellation::default(), active: Arc::default(), copy_cancel: Arc::default() });
        p.duration = 4.;
        p.review_fps = 1.;
        p.rendered_time = 1.;
        assert_eq!(p.total_fraction(), 0.25);
        let steps = |completed| ("Wonder".to_string(), 0, 1,
            abner_processor_api::FrameProgress { phase: "transformer".into(), completed, total: 63, frames: 0 });
        frame.lock().unwrap().report = Some(steps(21));
        assert!((p.total_fraction() - (1. + 21. / 63.) / 4.).abs() < 1e-12);
        // The call returned: the report clears before the frame lands.
        frame.lock().unwrap().report = None;
        assert!((p.total_fraction() - (1. + 21. / 63.) / 4.).abs() < 1e-12);
        p.rendered_time = 2.;
        assert_eq!(p.total_fraction(), 0.5);
        p.rendered_time = 4.;
        frame.lock().unwrap().report = Some(steps(63));
        assert_eq!(p.total_fraction(), 1.);
    }

    #[test]
    fn export_queue_runs_one_worker_at_a_time_and_can_cancel_waiting_job() {
        let mut p = Page::default();
        let (tx, done) = mpsc::channel();
        p.job = Some(Job { launch: None, latest: Arc::default(), stage: Arc::default(), frame: Arc::default(), done,
            cancel: Cancellation::default(), active: Arc::default(), copy_cancel: Arc::default() });
        p.archive();
        let (tx2, done) = mpsc::channel();
        let launched = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = launched.clone();
        p.job = Some(Job { launch: Some(Box::new(move || { flag.store(true, std::sync::atomic::Ordering::SeqCst); let _ = tx2.send(Ok(PathBuf::from("second.mp4"))); })),
            latest: Arc::default(), stage: Arc::default(), frame: Arc::default(), done, cancel: Cancellation::default(), active: Arc::default(), copy_cancel: Arc::default() });
        p.poll(&mut vec![], 3);
        assert!(!launched.load(std::sync::atomic::Ordering::SeqCst));
        p.cancel(); assert!(!p.busy());
        tx.send(Ok(PathBuf::from("first.mp4"))).unwrap();
        p.poll(&mut vec![], 3);
        assert!(!p.has_jobs()); assert!(!launched.load(std::sync::atomic::Ordering::SeqCst));
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
        p.preview_source = p.source.clone();
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
            launch: None,
            latest: latest.clone(),
            stage: Arc::default(),
            frame: Arc::default(),
            done,
            cancel: Cancellation::default(),
            active: Arc::default(),
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
        assert_eq!(render_line(0., 0, None, 20., 90., Some(&stage("Window 1 · encoding", 9, Some(18))), None),
            "0% · 0 frames · Example · Window 1 · encoding 50%");
        // 120 of 480 frames in ten minutes: five seconds a frame, thirty minutes to go.
        assert_eq!(render_line(0.25, 120, Some(1. / 24.), 20., 600., Some(&stage("Window 25 · restoring", 16, Some(32))), None),
            "25% · 120 of 480 frames · 5s per frame · ~30m 00s left · Example · Window 25 · restoring 50%");
        assert_eq!(render_line(0.5, 240, Some(1. / 24.), 20., 10., None, None),
            "50% · 240 of 480 frames · 24.0 fps · ~10s left");
        assert_eq!(render_line(0.5, 1, Some(0.5), 1., 4000., Some(&stage(" ", 3, None)), None),
            "50% · 1 of 2 frames · 1h 06m per frame · ~1h 06m left · Example · Processing");
        // A slow processor's steps replace its phase: the frame in hand.
        let frame = ("Wonder".to_string(), 0, 1, abner_processor_api::FrameProgress { phase: "transformer".into(), completed: 21, total: 63, frames: 0 });
        assert_eq!(render_line(0.5, 1, Some(0.5), 1., 60., Some(&stage("Tiles", 12, Some(12))), Some(&frame)),
            "50% · 1 of 2 frames · 1m 00s per frame · ~1m 00s left · Wonder · transformer 21/63");
        // A rounded total never reads below the frames already written.
        assert_eq!(render_line(1., 481, Some(1. / 24.), 20., 10., None, None), "100% · 481 of 481 frames · 48.1 fps");
    }
    #[test]
    fn export_controls_share_hit_bounds_at_supported_window_sizes() {
        let p = Page::default();
        for vp in [(720.,480.),(900.,600.),(1280.,800.),(1440.,900.)] {
            let l=Layout::new(vp);
            assert_eq!(l.video.y,crate::app::HEADER_H);
            assert!(l.canvas.button_bar.is_none());
            assert_eq!(p.actions(vp).iter().filter(|(..,a)|*a==Action::Export).count(),1);
            let actions=p.actions(vp);
            let play=actions.iter().find(|(..,a)|*a==Action::Play).unwrap().0;
            let card=l.job_card();
            assert!(play.y+play.h <= l.seek.y && l.seek.y+l.seek.h+12. <= card.y, "transport, track, then card");
            assert!(card.y+card.h <= l.footer.y, "card inside the attachment at {vp:?}");
            for (i,(r,_,_,a)) in actions.iter().enumerate() {
                assert!(r.x>=0. && r.x+r.w<=vp.0+0.1,"{a:?} at {vp:?}");
                assert!(r.y>=crate::app::HEADER_H && r.y+r.h<=l.footer.y,"{a:?} at {vp:?}");
                assert!(!hit(l.video,r.x,r.y));
                assert_eq!(p.click(vp,r.x+r.w/2.,r.y+r.h/2.),Some(*a));
                for (other,_,_,_) in &actions[i+1..] {
                    assert!(!(r.x<other.x+other.w && r.x+r.w>other.x && r.y<other.y+other.h && r.y+r.h>other.y),"overlapping control {a:?}");
                }
            }
            assert_eq!(p.click(vp,l.seek.x+l.seek.w/2.,l.seek.y),Some(Action::Seek(0.5)));
        }
    }
    #[test]
    fn reveal_is_in_job_card_and_export_remains_available() {
        let mut p=Page::default();p.source=Some(PathBuf::from("/clips/source.mp4"));
        assert_eq!(p.apply(Action::RevealDirectory),Some(PathBuf::from("/clips")));
        p.destination=Some(PathBuf::from("/exports/output.mp4"));p.path=p.destination.clone();
        assert_eq!(p.apply(Action::RevealDirectory),Some(PathBuf::from("/exports")));
        for vp in [(720.,480.),(1280.,800.)] {
            let actions=p.actions(vp);
            let play=actions.iter().find(|(..,a)|*a==Action::Play).unwrap().0;
            let l=Layout::new(vp);
            assert!(play.y+play.h <= l.job_card().y, "card sits under the transport");
            let reveal=actions.iter().find(|(..,a)|*a==Action::Reveal).unwrap();
            assert!(hit(l.job_card(),reveal.0.x,reveal.0.y));
            let card=l.job_card();
            assert!(reveal.0.y+reveal.0.h <= card.y+card.h-if card.h<110.{32.}else{42.});
            assert_eq!(p.click(vp,reveal.0.x+1.,reveal.0.y+1.),Some(Action::Reveal));
            assert_eq!(actions.iter().filter(|(..,a)|*a==Action::Export).count(),1);
        }
        assert_eq!(p.apply(Action::Reveal),p.destination);
    }
    #[test]
    fn preview_navigation_uses_available_boundaries_without_a_decoder() {
        let mut p=Page::default();
        for i in 0..3 { p.frames.push_back(Rendered{pixels:Arc::new(vec![0;4]),dims:[1,1],source_pts:i as f64,output_time:i as f64+1.,count:i,frame_duration:1.}); }
        p.apply(Action::Keyframe(true));assert_eq!(p.current().unwrap().count,2);
        p.apply(Action::Keyframe(false));assert_eq!(p.current().unwrap().count,0);
        p.apply(Action::Step(true));assert_eq!(p.current().unwrap().count,1);
        assert!(!p.following);
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
        let chooser = actions.iter().find(|(.., a)| *a == Action::ChooseDirectory).unwrap().0;
        assert!(chooser.y >= field.y + field.h);
        assert_eq!(chooser.x + chooser.w, field.x + field.w);
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
                Arc::new(move |name, update| if let StageUpdate::Phase(p) = update {
                    stages.lock().unwrap().push((name.to_owned(), p.phase, p.total))
                })
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
                Some(40_000),
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
        if !prores_only {
            let dest = dir.join("explicit-hevc.mp4");
            let _ = std::fs::remove_file(&dest);
            let mut count = 0;
            crate::media_io::pipeline_export_with_inputs(&Default::default(), &info, &dest,
                &[(0.,0.5)], &[], &registry, &[], false, Some(12_000), None,
                &Cancellation::default(), &Mutex::default(), None, |_| { count += 1; Ok(()) }).unwrap();
            assert_eq!(count, 12, "explicit HEVC must encode even with no processing stages");
            assert!(crate::probe::probe(&dest).unwrap().codec.to_lowercase().contains("hevc"));
            let mut page = Page::default(); page.path = Some(dest.clone()); page.open = true;
            page.open_output();
            let deadline = Instant::now() + std::time::Duration::from_secs(3);
            while page.current().is_none() && Instant::now() < deadline {
                page.poll(&mut vec![], 3); std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(page.current().is_some(), "completed export should decode in the viewing pane");
            page.apply(Action::Seek(0.6));
            while page.review_pending && Instant::now() < deadline {
                page.poll(&mut vec![], 3); std::thread::sleep(std::time::Duration::from_millis(5));
            }
            assert!(!page.review_pending);
            assert!(Page::frame_time(page.current().unwrap()) >= 0.29);
            drop(page);
            std::fs::remove_file(dest).unwrap();
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

    /// An interpolation-shaped stage: float RGB in and out, two output frames per
    /// input, and `ABNER_BENCH_WORK_MS` of simulated accelerator time per output.
    struct Interp;
    struct InterpSession { dims: [usize; 2], work: std::time::Duration, cancel: Cancellation }
    impl Factory for Interp {
        fn descriptor(&self) -> Descriptor {
            Descriptor { id: "test.interp".into(), name: "Interp".into(),
                settings_schema: serde_json::json!({}), presets: vec![] }
        }
        fn output_rate(&self, _: &serde_json::Value, input: abner_processor_api::Rate)
            -> Result<abner_processor_api::Rate, String> { Ok([input[0] * 2, input[1]]) }
        fn create(&self, w: usize, h: usize, _: &serde_json::Value) -> Result<Box<dyn Session>, String> {
            let ms = std::env::var("ABNER_BENCH_WORK_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
            Ok(Box::new(InterpSession { dims: [w, h], work: std::time::Duration::from_millis(ms),
                cancel: Cancellation::default() }))
        }
    }
    impl Session for InterpSession {
        fn format_pairs(&self) -> Vec<abner_processor_api::FormatPair> {
            vec![abner_processor_api::FormatPair::same(abner_processor_api::PixelFormat::RgbF32)]
        }
        fn configure_format(&mut self, _: abner_processor_api::FormatPair) -> Result<(), String> { Ok(()) }
        fn process_video(&mut self, mut f: abner_processor_api::VideoFrame)
            -> Result<Vec<abner_processor_api::VideoFrame>, String> {
            std::thread::sleep(self.work * 2);
            let half = f.duration / 2;
            let mut second = f.clone();
            second.pts += half;
            second.duration -= half;
            f.duration = half;
            Ok(vec![f, second])
        }
        fn finish_video(&mut self) -> Result<Vec<abner_processor_api::VideoFrame>, String> { Ok(vec![]) }
        fn output_dimensions(&self) -> [usize; 2] { self.dims }
        fn cancellation(&self) -> Cancellation { self.cancel.clone() }
        fn progress(&self) -> Progress { Progress { phase: "test".into(), completed: 0, total: None } }
    }

    /// `cargo test --release export_throughput -- --ignored --nocapture`
    /// (`ABNER_BENCH_SIZE`, default 3840x2160; `ABNER_BENCH_WORK_MS`, default 0).
    #[test]
    #[ignore]
    fn export_throughput() {
        let size = std::env::var("ABNER_BENCH_SIZE").unwrap_or_else(|_| "3840x2160".into());
        let dir = std::env::temp_dir().join(format!("abner-bench-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join(format!("bench-{size}.mp4"));
        assert!(std::process::Command::new("ffmpeg")
            .args(["-y", "-v", "error", "-f", "lavfi", "-i"])
            .arg(format!("testsrc2=size={size}:rate=24:duration=2"))
            .args(["-c:v", "libx264", "-preset", "ultrafast", "-pix_fmt", "yuv420p", "-g", "24"])
            .arg(&input).status().unwrap().success());
        let info = crate::probe::probe(&input).unwrap();
        let mut registry = Registry::default();
        registry.register(Box::new(Interp)).unwrap();
        for (label, chain) in [("hevc, no stages", vec![]),
            ("hevc, interp stage", vec![("test.interp".to_string(), serde_json::json!({}))])] {
            let dest = dir.join("out.mp4");
            let _ = std::fs::remove_file(&dest);
            let mut frames = 0;
            let start = Instant::now();
            crate::media_io::pipeline_export_with_inputs(&Default::default(), &info, &dest,
                &[(0., 2.)], &[], &registry, &chain, false, Some(40_000), None,
                &Cancellation::default(), &Mutex::default(), None, |_| { frames += 1; Ok(()) }).unwrap();
            let s = start.elapsed().as_secs_f64();
            eprintln!("{size} {label}: {frames} frames in {s:.2}s = {:.1} fps", frames as f64 / s);
        }
        let _ = std::fs::remove_dir_all(dir);
    }
}
